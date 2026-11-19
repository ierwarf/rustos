//! Exact-generation user-memory copy and range admission.
//!
//! - **Owner:** `kernel-ps` owns process-bound user-copy substrate.
//! - **Boundary:** Every user pointer, length, direction, and typed layout is
//!   untrusted.
//! - **Lifecycle:** Retain the process generation, validate the complete live
//!   page span, copy all bytes or none, then release the hold.
//! - **Concurrency:** Exec/exit cannot reclaim the retained address space while
//!   a copy is active; no service call occurs under page-table access.
//! - **Failure:** Noncanonical, overflowing, unmapped, permission, alignment,
//!   and short-span errors return before caller state publication.
//! - **Forbidden:** No raw slice construction before admission, partial
//!   authority copy, or foreign-process access by PID alone.
//! - **Evidence:** `user-memory-access`.
use nucleus_core::util::lockdep::{self, LockClass};
use x86_64::VirtAddr;

use super::usermem_profile;
use crate::arch::user_access;
use crate::memory::paging;
use crate::multitask;

const USER_C_STRING_COPY_CHUNK: usize = 256;
const USER_PAGE_SIZE: usize = 4096;

#[track_caller]
pub fn current_user_address_space()
-> Result<multitask::RetainedCurrentUserAddressSpace, paging::AddressSpaceError> {
    multitask::current_user_address_space().ok_or(paging::AddressSpaceError::NotMapped)
}

#[track_caller]
fn with_current_address_space<R>(
    f: impl FnOnce(&paging::ProcessAddressSpace) -> Result<R, paging::AddressSpaceError>,
) -> Result<R, paging::AddressSpaceError> {
    multitask::with_current_mm(f).ok_or(paging::AddressSpaceError::NotMapped)?
}

#[track_caller]
fn with_current_write_address_space<R>(
    f: impl FnOnce(&multitask::UserCopyAddressSpace<'_>) -> Result<R, paging::AddressSpaceError>,
) -> Result<R, paging::AddressSpaceError> {
    multitask::with_current_user_copy_mm(f).ok_or(paging::AddressSpaceError::NotMapped)?
}

/// Faults in missing lazy anonymous pages across `ranges`, returning `Ok(true)`
/// if any page is a committed anonymous mapping that is now resident.
fn populate_anonymous_ranges(
    ranges: &[(u64, usize)],
    write: bool,
) -> Result<bool, paging::AddressSpaceError> {
    const RETRIES: usize = 256;
    let mut populated_any = false;
    for &(user_ptr, len) in ranges {
        if len == 0 {
            continue;
        }
        let start = user_virt_addr(user_ptr, len)?;
        let mut page = start.as_u64() & !(USER_PAGE_SIZE as u64 - 1);
        let last_page = (start.as_u64() + len as u64 - 1) & !(USER_PAGE_SIZE as u64 - 1);
        while page <= last_page {
            let mut attempts = 0usize;
            loop {
                match kernel_hal::api::populate_current_anonymous_page(page, write) {
                    kernel_hal::api::AnonymousPopulateStatus::Resident => {
                        populated_any = true;
                        break;
                    }
                    kernel_hal::api::AnonymousPopulateStatus::Retry if attempts < RETRIES => {
                        attempts += 1;
                        core::hint::spin_loop();
                    }
                    kernel_hal::api::AnonymousPopulateStatus::Retry
                    | kernel_hal::api::AnonymousPopulateStatus::NoMemory => {
                        return Err(paging::AddressSpaceError::OutOfFrames);
                    }
                    kernel_hal::api::AnonymousPopulateStatus::NotPopulatable => break,
                }
            }
            page = page.saturating_add(USER_PAGE_SIZE as u64);
        }
    }
    Ok(populated_any)
}

fn populate_anonymous_range(
    user_ptr: u64,
    len: usize,
    write: bool,
) -> Result<bool, paging::AddressSpaceError> {
    populate_anonymous_ranges(&[(user_ptr, len)], write)
}

pub fn copy_from_current_user_exact(
    user_ptr: u64,
    dest: &mut [u8],
) -> Result<(), paging::AddressSpaceError> {
    let dest_len = dest.len();
    let mut run = || {
        let entry = usermem_profile::now();
        with_current_address_space(|address_space| {
            let bound = usermem_profile::charge(usermem_profile::UserCopyPhase::ReadBind, entry);
            let start = user_virt_addr(user_ptr, dest.len())?;
            let validated_read = address_space.validate_user_read(start, dest.len())?;
            let validated =
                usermem_profile::charge(usermem_profile::UserCopyPhase::ReadValidate, bound);
            let result = validated_read.copy_into(dest);
            usermem_profile::charge(usermem_profile::UserCopyPhase::ReadCopy, validated);
            result
        })
    };
    match run() {
        Err(paging::AddressSpaceError::NotMapped)
            if populate_anonymous_range(user_ptr, dest_len, false)? =>
        {
            run()
        }
        other => other,
    }
}

/// Performs the fast-call request copy and reply-range admission under one
/// exact MM-generation bind. No IPC state is visible yet, so both operations
/// remain preflight; combining only removes a duplicate process/table lock.
pub fn copy_from_retained_user_and_validate_write(
    retained: &multitask::RetainedCurrentUserAddressSpace,
    read_ptr: u64,
    dest: &mut [u8],
    write_ptr: u64,
    write_len: usize,
) -> Result<(), paging::AddressSpaceError> {
    copy_from_retained_user_and_validate_writes(retained, read_ptr, dest, &[(write_ptr, write_len)])
}

/// Copies one input and admits every output under one exact-MM state lock.
/// This is the reply-receive pre-commit shape: the old reply capability is
/// still live, so all user ranges must succeed before any IPC publication.
pub fn copy_from_retained_user_and_validate_writes(
    retained: &multitask::RetainedCurrentUserAddressSpace,
    read_ptr: u64,
    dest: &mut [u8],
    write_buffers: &[(u64, usize)],
) -> Result<(), paging::AddressSpaceError> {
    // Direct path: probe every output page, then read the input in place. A
    // probe writes nothing, so any fault leaves the validated path a clean start.
    if user_access::enabled() {
        let outputs_writable = write_buffers.iter().copied().all(|(write_ptr, write_len)| {
            write_len == 0
                || user_virt_addr(write_ptr, write_len)
                    .is_ok_and(|start| user_access::probe_write(start.as_u64(), write_len))
        });
        if outputs_writable
            && let Ok(start) = user_virt_addr(read_ptr, dest.len())
            && user_access::copy_from_user(dest, start.as_u64())
        {
            return Ok(());
        }
    }
    let dest_len = dest.len();
    let mut attempt = || {
        retained
            .try_with_user_copy_address_space(|address_space| {
                let mut memo = kernel_mm::api::address_space::UserTranslationMemo::new();
                for (write_ptr, write_len) in write_buffers.iter().copied() {
                    if write_len != 0 {
                        let write_start = user_virt_addr(write_ptr, write_len)?;
                        let _ =
                            address_space.validate_user_write_memo(write_start, write_len, &mut memo)?;
                    }
                }
                let read_start = user_virt_addr(read_ptr, dest.len())?;
                address_space
                    .validate_user_read_memo(read_start, dest.len(), &mut memo)?
                    .copy_into(dest)
            })
            .ok_or(paging::AddressSpaceError::NotMapped)?
    };
    match attempt() {
        Err(paging::AddressSpaceError::NotMapped) => {
            let pop_w = populate_anonymous_ranges(write_buffers, true)?;
            let pop_r = populate_anonymous_range(read_ptr, dest_len, false)?;
            if pop_w || pop_r {
                attempt()
            } else {
                Err(paging::AddressSpaceError::NotMapped)
            }
        }
        other => other,
    }
}

/// Several user-memory operations under ONE exact-MM bind. The binding (state
/// lock plus identity check) is most of a small copy's cost, so a syscall that
/// reads and admits several ranges before any publication does them in one
/// closure. The closure must not call a service or block.
pub struct UserAccess<'a, 'b> {
    /// `None` is the direct mode: the calling task's own live address space is
    /// accessed through the fault-recoverable copy routines, with no bind.
    space: Option<&'a multitask::UserCopyAddressSpace<'b>>,
    memo: core::cell::Cell<kernel_mm::api::address_space::UserTranslationMemo>,
    /// A direct access faulted; the closure result is discarded and the whole
    /// closure re-runs under the validated bind.
    faulted: core::cell::Cell<bool>,
}

impl UserAccess<'_, '_> {
    pub fn read_exact(&self, user_ptr: u64, dest: &mut [u8]) -> Result<(), paging::AddressSpaceError> {
        let start = user_virt_addr(user_ptr, dest.len())?;
        let Some(space) = self.space else {
            if user_access::copy_from_user(dest, start.as_u64()) {
                return Ok(());
            }
            self.faulted.set(true);
            return Err(paging::AddressSpaceError::NotMapped);
        };
        let mut memo = self.memo.get();
        let proof = space.validate_user_read_memo(start, dest.len(), &mut memo);
        self.memo.set(memo);
        proof?.copy_into(dest)
    }

    pub fn read_struct<T: Copy + Default>(&self, user_ptr: u64) -> Result<T, paging::AddressSpaceError> {
        let mut value = T::default();
        // SAFETY: `value` is live and uniquely borrowed for exactly
        // `size_of::<T>()` bytes; the copy initializes only that allocation.
        let bytes = unsafe {
            core::slice::from_raw_parts_mut(
                core::ptr::addr_of_mut!(value).cast::<u8>(),
                core::mem::size_of::<T>(),
            )
        };
        self.read_exact(user_ptr, bytes)?;
        Ok(value)
    }

    pub fn validate_write(&self, user_ptr: u64, len: usize) -> Result<(), paging::AddressSpaceError> {
        if len == 0 {
            return Ok(());
        }
        let start = user_virt_addr(user_ptr, len)?;
        let Some(space) = self.space else {
            if user_access::probe_write(start.as_u64(), len) {
                return Ok(());
            }
            self.faulted.set(true);
            return Err(paging::AddressSpaceError::NotMapped);
        };
        let mut memo = self.memo.get();
        let proof = space.validate_user_write_memo(start, len, &mut memo);
        self.memo.set(memo);
        proof.map(drop)
    }
}

/// Runs `f` against the caller's user memory. When the direct path is enabled
/// the closure first runs with no bind at all; if any direct access faults its
/// result is dropped and it re-runs once under the exact-MM bind, so the answer
/// (including COW resolution and the precise errno) is always the validated
/// one. The closure must therefore be idempotent and must not block.
pub fn with_retained_user_access<R>(
    retained: &multitask::RetainedCurrentUserAddressSpace,
    mut f: impl FnMut(&UserAccess<'_, '_>) -> R,
) -> Result<R, paging::AddressSpaceError> {
    if user_access::enabled() {
        let direct = UserAccess {
            space: None,
            memo: core::cell::Cell::new(Default::default()),
            faulted: core::cell::Cell::new(false),
        };
        let result = f(&direct);
        if !direct.faulted.get() {
            return Ok(result);
        }
    }
    retained
        .try_with_user_copy_address_space(|address_space| {
            f(&UserAccess {
                space: Some(address_space),
                memo: core::cell::Cell::new(Default::default()),
                faulted: core::cell::Cell::new(false),
            })
        })
        .ok_or(paging::AddressSpaceError::NotMapped)
}

pub fn copy_from_retained_user_exact(
    retained: &multitask::RetainedCurrentUserAddressSpace,
    user_ptr: u64,
    dest: &mut [u8],
) -> Result<(), paging::AddressSpaceError> {
    if user_access::enabled()
        && let Ok(start) = user_virt_addr(user_ptr, dest.len())
        && user_access::copy_from_user(dest, start.as_u64())
    {
        return Ok(());
    }
    let dest_len = dest.len();
    let mut attempt = || {
        retained
            .try_with_address_space(|address_space| {
                let start = user_virt_addr(user_ptr, dest.len())?;
                address_space
                    .validate_user_read(start, dest.len())?
                    .copy_into(dest)
            })
            .ok_or(paging::AddressSpaceError::NotMapped)?
    };
    match attempt() {
        Err(paging::AddressSpaceError::NotMapped)
            if populate_anonymous_range(user_ptr, dest_len, false)? =>
        {
            attempt()
        }
        other => other,
    }
}

pub fn read_current_user_struct<T: Copy + Default>(
    user_ptr: u64,
) -> Result<T, paging::AddressSpaceError> {
    let mut value = T::default();
    // SAFETY: `value` is live and uniquely borrowed for exactly `size_of::<T>()`
    // bytes; the byte copy initializes only that existing `T` allocation.
    let bytes = unsafe {
        core::slice::from_raw_parts_mut(
            core::ptr::addr_of_mut!(value).cast::<u8>(),
            core::mem::size_of::<T>(),
        )
    };
    copy_from_current_user_exact(user_ptr, bytes)?;
    Ok(value)
}

pub fn read_retained_user_struct<T: Copy + Default>(
    retained: &multitask::RetainedCurrentUserAddressSpace,
    user_ptr: u64,
) -> Result<T, paging::AddressSpaceError> {
    let mut value = T::default();
    // SAFETY: `value` is live and uniquely borrowed for exactly `size_of::<T>()`
    // bytes; the exact-length copy below overwrites only that `T` allocation
    // and fails without returning `value` if the user range is short.
    let bytes = unsafe {
        core::slice::from_raw_parts_mut(
            core::ptr::addr_of_mut!(value).cast::<u8>(),
            core::mem::size_of::<T>(),
        )
    };
    copy_from_retained_user_exact(retained, user_ptr, bytes)?;
    Ok(value)
}

pub fn write_current_user_bytes(
    user_ptr: u64,
    bytes: &[u8],
) -> Result<(), paging::AddressSpaceError> {
    let run = || {
        let entry = usermem_profile::now();
        with_current_write_address_space(|address_space| {
            let bound = usermem_profile::charge(usermem_profile::UserCopyPhase::WriteBind, entry);
            let start = user_virt_addr(user_ptr, bytes.len())?;
            let validated_write = address_space.validate_user_write(start, bytes.len())?;
            let validated =
                usermem_profile::charge(usermem_profile::UserCopyPhase::WriteValidate, bound);
            let result = validated_write.copy_from(bytes);
            usermem_profile::charge(usermem_profile::UserCopyPhase::WriteCopy, validated);
            result
        })
    };
    match run() {
        Err(paging::AddressSpaceError::NotMapped)
            if populate_anonymous_range(user_ptr, bytes.len(), true)? =>
        {
            run()
        }
        other => other,
    }
}

pub fn write_retained_user_bytes(
    retained: &multitask::RetainedCurrentUserAddressSpace,
    user_ptr: u64,
    bytes: &[u8],
) -> Result<(), paging::AddressSpaceError> {
    if user_access::enabled()
        && !bytes.is_empty()
        && let Ok(start) = user_virt_addr(user_ptr, bytes.len())
        && user_access::copy_to_user(start.as_u64(), bytes)
    {
        return Ok(());
    }
    let attempt = || {
        retained
            .try_with_user_copy_address_space(|address_space| {
                let start = user_virt_addr(user_ptr, bytes.len())?;
                address_space
                    .validate_user_write(start, bytes.len())?
                    .copy_from(bytes)
            })
            .ok_or(paging::AddressSpaceError::NotMapped)?
    };
    match attempt() {
        Err(paging::AddressSpaceError::NotMapped)
            if populate_anonymous_range(user_ptr, bytes.len(), true)? =>
        {
            attempt()
        }
        other => other,
    }
}

pub fn validate_current_user_write_buffer(
    user_ptr: u64,
    len: usize,
) -> Result<(), paging::AddressSpaceError> {
    let run = || {
        with_current_write_address_space(|address_space| {
            address_space.validate_user_write_buffer(user_virt_addr(user_ptr, len)?, len)
        })
    };
    match run() {
        Err(paging::AddressSpaceError::NotMapped)
            if populate_anonymous_range(user_ptr, len, true)? =>
        {
            run()
        }
        other => other,
    }
}

/// Admits several user write ranges under one address-space bind.
///
/// Binding is the expensive half of a small user access: it re-derives the
/// caller's identity and takes the per-process state lock, about 1,240 cycles,
/// against roughly 110 for the range check itself. A synchronous receive
/// admits a message buffer, a reply-capability word, and two sender-identity
/// words before it consumes anything, and doing that as four calls bound four
/// times.
///
/// Ranges are admitted in the given order and a zero-length range is skipped,
/// which is what the call sites expressed with an explicit length test. The
/// first rejection returns, so an unadmitted range is never reported as
/// admitted.
#[track_caller]
pub fn validate_current_user_write_buffers(
    buffers: &[(u64, usize)],
) -> Result<(), paging::AddressSpaceError> {
    if buffers.iter().all(|(_, len)| *len == 0) {
        return Ok(());
    }
    let attempt = || {
        // The whole point of the batched form is that the bind happens once. That
        // is not visible in the bytes it produces, so it gets an assertion rather
        // than a comment: an edit that reintroduces a per-range bind fails here
        // instead of costing 1,240 cycles a range until a benchmark notices.
        let _bind_budget = lockdep::work_budget::declare(
            LockClass::ProcessState,
            rustos_user_abi::performance::USER_COPY_BATCH_MAX_ADDRESS_SPACE_BINDS,
        );
        with_current_write_address_space(|address_space| {
            for (user_ptr, len) in buffers.iter().copied() {
                if len == 0 {
                    continue;
                }
                address_space.validate_user_write_buffer(user_virt_addr(user_ptr, len)?, len)?;
            }
            Ok(())
        })
    };
    match attempt() {
        Err(paging::AddressSpaceError::NotMapped)
            if populate_anonymous_ranges(buffers, true)? =>
        {
            attempt()
        }
        other => other,
    }
}

pub fn validate_retained_user_write_buffers(
    retained: &multitask::RetainedCurrentUserAddressSpace,
    buffers: &[(u64, usize)],
) -> Result<(), paging::AddressSpaceError> {
    let attempt = || {
        retained
            .try_with_user_copy_address_space(|address_space| {
                for (user_ptr, len) in buffers.iter().copied() {
                    if len != 0 {
                        address_space
                            .validate_user_write_buffer(user_virt_addr(user_ptr, len)?, len)?;
                    }
                }
                Ok(())
            })
            .ok_or(paging::AddressSpaceError::NotMapped)?
    };
    match attempt() {
        Err(paging::AddressSpaceError::NotMapped)
            if populate_anonymous_ranges(buffers, true)? =>
        {
            attempt()
        }
        other => other,
    }
}

/// Writes several user buffers under one address-space bind.
///
/// The same binding cost as [`validate_current_user_write_buffers`], against
/// copies of a few dozen bytes each.
///
/// Every buffer is validated immediately before it is copied, in the given
/// order, so a failure leaves exactly the prefix the equivalent sequence of
/// single writes would have left. A zero-length buffer is skipped.
#[track_caller]
pub fn write_current_user_bytes_batch(
    writes: &[(u64, &[u8])],
) -> Result<(), paging::AddressSpaceError> {
    if writes.iter().all(|(_, bytes)| bytes.is_empty()) {
        return Ok(());
    }
    let attempt = || {
        // As in `validate_current_user_write_buffers`: one bind for the whole
        // batch is the contract, so it is asserted.
        let _bind_budget = lockdep::work_budget::declare(
            LockClass::ProcessState,
            rustos_user_abi::performance::USER_COPY_BATCH_MAX_ADDRESS_SPACE_BINDS,
        );
        let entry = usermem_profile::now();
        with_current_write_address_space(|address_space| {
            let mut phase = usermem_profile::charge(usermem_profile::UserCopyPhase::WriteBind, entry);
            for (user_ptr, bytes) in writes.iter().copied() {
                if bytes.is_empty() {
                    continue;
                }
                let start = user_virt_addr(user_ptr, bytes.len())?;
                let validated_write = address_space.validate_user_write(start, bytes.len())?;
                phase =
                    usermem_profile::charge(usermem_profile::UserCopyPhase::WriteValidate, phase);
                validated_write.copy_from(bytes)?;
                phase = usermem_profile::charge(usermem_profile::UserCopyPhase::WriteCopy, phase);
            }
            Ok(())
        })
    };
    match attempt() {
        Err(paging::AddressSpaceError::NotMapped) => {
            let mut populated = false;
            for &(user_ptr, bytes) in writes {
                if !bytes.is_empty() && populate_anonymous_range(user_ptr, bytes.len(), true)? {
                    populated = true;
                }
            }
            if populated {
                attempt()
            } else {
                Err(paging::AddressSpaceError::NotMapped)
            }
        }
        other => other,
    }
}

pub fn write_retained_user_bytes_batch(
    retained: &multitask::RetainedCurrentUserAddressSpace,
    writes: &[(u64, &[u8])],
) -> Result<(), paging::AddressSpaceError> {
    if user_access::enabled() {
        let direct = writes.iter().copied().all(|(user_ptr, bytes)| {
            bytes.is_empty()
                || user_virt_addr(user_ptr, bytes.len())
                    .is_ok_and(|start| user_access::copy_to_user(start.as_u64(), bytes))
        });
        if direct {
            return Ok(());
        }
    }
    let attempt = || {
        retained
            .try_with_user_copy_address_space(|address_space| {
                let mut memo = kernel_mm::api::address_space::UserTranslationMemo::new();
                for (user_ptr, bytes) in writes.iter().copied() {
                    if bytes.is_empty() {
                        continue;
                    }
                    let start = user_virt_addr(user_ptr, bytes.len())?;
                    address_space
                        .validate_user_write_memo(start, bytes.len(), &mut memo)?
                        .copy_from(bytes)?;
                }
                Ok(())
            })
            .ok_or(paging::AddressSpaceError::NotMapped)?
    };
    match attempt() {
        Err(paging::AddressSpaceError::NotMapped) => {
            let mut populated = false;
            for &(user_ptr, bytes) in writes {
                if !bytes.is_empty() && populate_anonymous_range(user_ptr, bytes.len(), true)? {
                    populated = true;
                }
            }
            if populated {
                attempt()
            } else {
                Err(paging::AddressSpaceError::NotMapped)
            }
        }
        other => other,
    }
}

/// Copies `value` to user memory. `T: UserWire` proves the value has no padding,
/// so no uninitialised kernel byte can reach user space.
pub fn write_current_user_struct<T: rustos_user_abi::wire::UserWire>(
    user_ptr: u64,
    value: &T,
) -> Result<(), paging::AddressSpaceError> {
    // SAFETY: `T: UserWire` guarantees every byte of `*value` is initialised,
    // and the slice borrows `value` for the duration of the copy.
    let bytes = unsafe {
        core::slice::from_raw_parts(
            core::ptr::addr_of!(*value).cast::<u8>(),
            core::mem::size_of::<T>(),
        )
    };
    write_current_user_bytes(user_ptr, bytes)
}

/// Copies a batch of padding-free wire records to user memory in one pass.
pub fn write_current_user_slice<T: rustos_user_abi::wire::UserWire>(
    user_ptr: u64,
    values: &[T],
) -> Result<(), paging::AddressSpaceError> {
    // SAFETY: `T: UserWire` guarantees every byte of each element is
    // initialised; the slice covers exactly `values` and borrows it.
    let bytes = unsafe {
        core::slice::from_raw_parts(
            values.as_ptr().cast::<u8>(),
            core::mem::size_of_val(values),
        )
    };
    write_current_user_bytes(user_ptr, bytes)
}

pub fn write_current_user_u32(user_ptr: u64, value: u32) -> Result<(), paging::AddressSpaceError> {
    write_current_user_bytes(user_ptr, &value.to_le_bytes())
}

pub fn read_current_user_u32(user_ptr: u64) -> Result<u32, paging::AddressSpaceError> {
    let mut bytes = [0_u8; 4];
    copy_from_current_user_exact(user_ptr, &mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

pub fn read_current_user_c_string(
    user_ptr: u64,
    max_len: usize,
) -> Result<alloc::string::String, paging::AddressSpaceError> {
    read_current_user_c_string_bounded(user_ptr, max_len)?
        .ok_or(paging::AddressSpaceError::AddressOverflow)
}

/// Returns `None` only if all admitted bytes were readable but none was NUL.
pub fn read_current_user_c_string_bounded(
    user_ptr: u64,
    max_len: usize,
) -> Result<Option<alloc::string::String>, paging::AddressSpaceError> {
    let max_pages = max_len.div_ceil(USER_PAGE_SIZE).saturating_add(1);
    for _ in 0..max_pages {
        let mut fault_ptr = None;
        let result = with_current_address_space(|address_space| {
            let mut bytes = alloc::vec::Vec::new();
            let mut chunk = [0_u8; USER_C_STRING_COPY_CHUNK];

            while bytes.len() < max_len {
                let ptr = user_ptr
                    .checked_add(bytes.len() as u64)
                    .ok_or(paging::AddressSpaceError::AddressOverflow)?;
                let page_remaining = USER_PAGE_SIZE - ((ptr as usize) & (USER_PAGE_SIZE - 1));
                let chunk_len = (max_len - bytes.len()).min(chunk.len()).min(page_remaining);
                let start = user_virt_addr(ptr, chunk_len)?;
                if let Err(err) = address_space.copy_from_user(start, &mut chunk[..chunk_len]) {
                    if matches!(err, paging::AddressSpaceError::NotMapped) {
                        fault_ptr = Some(ptr);
                    }
                    return Err(err);
                }
                if let Some(nul_index) = chunk[..chunk_len].iter().position(|byte| *byte == 0) {
                    bytes.extend_from_slice(&chunk[..nul_index]);
                    return Ok(Some(alloc::string::String::from_utf8_lossy(&bytes).into_owned()));
                }
                bytes.extend_from_slice(&chunk[..chunk_len]);
            }

            Ok(None)
        });
        match result {
            Err(paging::AddressSpaceError::NotMapped) => {
                if let Some(ptr) = fault_ptr
                    && populate_anonymous_range(ptr, 1, false)?
                {
                    continue;
                }
                return Err(paging::AddressSpaceError::NotMapped);
            }
            other => return other,
        }
    }
    Err(paging::AddressSpaceError::NotMapped)
}

fn user_virt_addr(user_ptr: u64, len: usize) -> Result<VirtAddr, paging::AddressSpaceError> {
    if len == 0 {
        return Ok(VirtAddr::new(paging::USER_SPACE_BASE));
    }
    let last = user_ptr
        .checked_add(len as u64 - 1)
        .ok_or(paging::AddressSpaceError::AddressOverflow)?;
    if user_ptr < paging::USER_SPACE_BASE || last >= paging::USER_SPACE_END_EXCLUSIVE {
        return Err(paging::AddressSpaceError::AddressOutOfRange);
    }
    Ok(VirtAddr::new(user_ptr))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_virt_addr_rejects_out_of_range_without_panicking() {
        assert_eq!(
            user_virt_addr(paging::USER_SPACE_BASE, 1).map(VirtAddr::as_u64),
            Ok(paging::USER_SPACE_BASE)
        );
        assert_eq!(
            user_virt_addr(paging::USER_SPACE_END_EXCLUSIVE, 1),
            Err(paging::AddressSpaceError::AddressOutOfRange)
        );
        assert_eq!(
            user_virt_addr(u64::MAX, 8),
            Err(paging::AddressSpaceError::AddressOverflow)
        );
    }

    #[test]
    fn populate_anonymous_ranges_ignores_empty_and_checks_bounds() {
        assert_eq!(populate_anonymous_ranges(&[(0, 0)], true), Ok(false));
        assert_eq!(
            populate_anonymous_ranges(&[(paging::USER_SPACE_END_EXCLUSIVE, 1)], true),
            Err(paging::AddressSpaceError::AddressOutOfRange)
        );
        assert_eq!(
            populate_anonymous_ranges(&[(paging::USER_SPACE_BASE, 4096)], false),
            Ok(false)
        );
    }
}
