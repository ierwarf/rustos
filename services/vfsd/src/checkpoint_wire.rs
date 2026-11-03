//! Pure checkpoint-record and cache-range encodings owned by vfsd.
//!
//! These live in the library target because the `vfsd` binary is
//! `no_std`/`no_main` and its test harness is disabled; a `#[test]` kept beside
//! the service loop would never execute.

use core::mem::size_of;

use rustos_user_abi::syscall::{
    waitset_interest_shape_valid, ServiceCheckpointRecordWire, WaitSetInterestWire,
    SERVICE_CHECKPOINT_FLAG_TOMBSTONE, WAITSET_ABI_VERSION,
};

use crate::{WaitSetInterestKey, WaitSetInterestRecord};

/// Checkpoint key tag of an epoll object (and parent tag of its interests).
pub const CHECKPOINT_EPOLL_TAG: u64 = 1;

/// Copies the cached file bytes at `start..` into `dest`; an offset at or past
/// EOF (or beyond `usize`) copies nothing.
pub fn copy_file_cache_range(bytes: &[u8], start: u64, dest: &mut [u8]) -> usize {
    let Ok(start) = usize::try_from(start) else {
        return 0;
    };
    let Some(source) = bytes.get(start..) else {
        return 0;
    };
    let count = source.len().min(dest.len());
    dest[..count].copy_from_slice(&source[..count]);
    count
}

pub fn checkpoint_epoll_record(token: u64, tombstone: bool) -> ServiceCheckpointRecordWire {
    let mut record = ServiceCheckpointRecordWire {
        key_hi: token,
        key_lo: CHECKPOINT_EPOLL_TAG,
        ..ServiceCheckpointRecordWire::default()
    };
    if tombstone {
        record.flags = SERVICE_CHECKPOINT_FLAG_TOMBSTONE;
    }
    record
}

pub fn checkpoint_interest_record(
    epoll_token: u64,
    interest: WaitSetInterestRecord,
    tombstone: bool,
) -> ServiceCheckpointRecordWire {
    let (key_hi, key_lo) = checkpoint_interest_key(&interest);
    let mut record = ServiceCheckpointRecordWire {
        key_hi,
        key_lo,
        parent_hi: epoll_token,
        parent_lo: CHECKPOINT_EPOLL_TAG,
        ..ServiceCheckpointRecordWire::default()
    };
    if tombstone {
        record.flags = SERVICE_CHECKPOINT_FLAG_TOMBSTONE;
        return record;
    }
    let wire = WaitSetInterestWire {
        abi_version: WAITSET_ABI_VERSION,
        provider: interest.key.provider,
        flags: 0,
        target_fd: interest.key.target_fd,
        object_id: interest.key.object_id,
        provider_epoch: interest.provider_epoch,
        events: interest.events,
        reserved0: 0,
        data: interest.data,
    };
    // SAFETY: `WaitSetInterestWire` is a `repr(C)` plain-data wire struct that
    // lives for this whole function; viewing it as bytes reads only its own
    // `size_of` bytes.
    let bytes = unsafe {
        core::slice::from_raw_parts(
            (&wire as *const WaitSetInterestWire).cast::<u8>(),
            size_of::<WaitSetInterestWire>(),
        )
    };
    record.value_len = bytes.len() as u32;
    record.value[..bytes.len()].copy_from_slice(bytes);
    record
}

pub fn checkpoint_interest_key(interest: &WaitSetInterestRecord) -> (u64, u64) {
    (
        interest.key.object_id,
        (u64::from(interest.key.provider) << 48) | interest.key.target_fd,
    )
}

pub fn waitset_interest_from_wire(wire: &WaitSetInterestWire) -> Option<WaitSetInterestRecord> {
    if !waitset_interest_shape_valid(wire) {
        return None;
    }
    Some(WaitSetInterestRecord {
        key: WaitSetInterestKey {
            target_fd: wire.target_fd,
            provider: wire.provider,
            object_id: wire.object_id,
        },
        provider_epoch: wire.provider_epoch,
        events: wire.events,
        data: wire.data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustos_user_abi::linux as linux_abi;

    #[test]
    fn waitset_checkpoint_keys_are_parent_scoped_and_round_trip_wire() {
        let interest = WaitSetInterestRecord {
            key: WaitSetInterestKey {
                target_fd: 7,
                provider: rustos_user_abi::syscall::WAITSET_PROVIDER_NETD,
                object_id: 0xfeed_beef,
            },
            provider_epoch: 9,
            events: linux_abi::EPOLLIN,
            data: 11,
        };
        let record = checkpoint_interest_record(44, interest, false);
        assert_eq!(record.parent_hi, 44);
        assert_eq!(record.parent_lo, CHECKPOINT_EPOLL_TAG);
        assert_eq!(record.value_len as usize, size_of::<WaitSetInterestWire>());
        // SAFETY: `value` holds `value_len` bytes copied from a complete
        // `WaitSetInterestWire`; `read_unaligned` tolerates the byte array's
        // alignment.
        let wire = unsafe {
            core::ptr::read_unaligned(record.value.as_ptr().cast::<WaitSetInterestWire>())
        };
        assert_eq!(waitset_interest_from_wire(&wire), Some(interest));
        assert_eq!(size_of::<ServiceCheckpointRecordWire>(), 128);
    }

    #[test]
    fn epoll_checkpoint_records_store_liveness_not_descriptor_refcounts() {
        let live = checkpoint_epoll_record(44, false);
        assert_eq!(live.key_hi, 44);
        assert_eq!(live.key_lo, CHECKPOINT_EPOLL_TAG);
        assert_eq!(live.value_len, 0);
        assert_eq!(live.flags & SERVICE_CHECKPOINT_FLAG_TOMBSTONE, 0);

        let retired = checkpoint_epoll_record(44, true);
        assert_ne!(retired.flags & SERVICE_CHECKPOINT_FLAG_TOMBSTONE, 0);
        assert_eq!(retired.value_len, 0);
    }

    #[test]
    fn cached_file_ranges_are_exact_and_eof_bounded() {
        let bytes = b"0123456789";
        let mut out = [0xaa; 4];
        assert_eq!(copy_file_cache_range(bytes, 3, &mut out), 4);
        assert_eq!(&out, b"3456");
        assert_eq!(copy_file_cache_range(bytes, 9, &mut out), 1);
        assert_eq!(out[0], b'9');
        assert_eq!(copy_file_cache_range(bytes, 10, &mut out), 0);
        assert_eq!(copy_file_cache_range(bytes, u64::MAX, &mut out), 0);
    }
}
