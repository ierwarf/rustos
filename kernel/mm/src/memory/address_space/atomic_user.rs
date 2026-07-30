//! Atomic access to process-owned futex words.
//!
//! - **Owner:** `kernel-mm` owns atomic translation and access; kernel-compat
//!   owns Linux futex cleanup policy.
//! - **Boundary:** Only an aligned, complete, present, user-accessible,
//!   writable `u32` in the retained process mapping is admitted.
//! - **Lifecycle:** Retain the mapping generation, resolve once, perform one
//!   atomic operation, then release the process-state guard.
//! - **Concurrency:** Callers retain the exact process state lock, preventing
//!   page-table mutation while user CPUs may concurrently access the word.
//! - **Failure:** Mapping/protection errors are returned; impossible physical
//!   misalignment is a kernel invariant panic.
//! - **Forbidden:** No byte-copy read/modify/write, unaligned atomic, escaped
//!   raw pointer, or operation after process-generation release.
//! - **Evidence:** `thread-exit-futex-cleanup` and
//!   `robust-futex-owner-death/RobustFutexOwnerDeath`.

use core::sync::atomic::{AtomicU32, Ordering};

use x86_64::VirtAddr;

use super::{
    AddressSpaceError, ProcessAddressSpace, USER_SPACE_BASE, USER_SPACE_END_EXCLUSIVE,
    UserBufferAccess, higher_half_ptr, validate_user_page_access,
};

impl ProcessAddressSpace {
    pub fn atomic_load_user_u32(&self, address: u64) -> Result<u32, AddressSpaceError> {
        let word = self.atomic_user_u32(address)?;
        // ORDERING: Acquire observes release publication by another CPU before
        // the kernel decides whether this thread still owns the futex.
        Ok(word.load(Ordering::Acquire))
    }

    pub fn atomic_compare_exchange_user_u32(
        &self,
        address: u64,
        current: u32,
        new: u32,
    ) -> Result<Result<u32, u32>, AddressSpaceError> {
        let word = self.atomic_user_u32(address)?;
        // ORDERING: AcqRel publishes OWNER_DIED before a waiter is woken and
        // Acquire returns the exact competing user value on CAS failure.
        Ok(word.compare_exchange(current, new, Ordering::AcqRel, Ordering::Acquire))
    }

    pub fn atomic_store_user_u32_release(
        &self,
        address: u64,
        value: u32,
    ) -> Result<(), AddressSpaceError> {
        let word = self.atomic_user_u32(address)?;
        // ORDERING: Release makes clear_child_tid visible before futex wake.
        word.store(value, Ordering::Release);
        Ok(())
    }

    fn atomic_user_u32(&self, address: u64) -> Result<&AtomicU32, AddressSpaceError> {
        validate_atomic_u32_address(address)?;
        let virt = VirtAddr::new(address);
        let (phys, flags) = self
            .translate_user_with_flags(virt)
            .ok_or(AddressSpaceError::NotMapped)?;
        validate_user_page_access(flags, UserBufferAccess::Write)?;
        assert!(
            phys.as_u64()
                .is_multiple_of(core::mem::align_of::<AtomicU32>() as u64),
            "user atomic invariant: aligned virtual futex translated to misaligned physical word"
        );
        let pointer = higher_half_ptr(phys).cast::<AtomicU32>();
        // SAFETY: the caller holds the process-state lock, admission proved a
        // present writable page and natural alignment, and the direct map
        // retains the physical frame for the complete atomic operation.
        Ok(unsafe { &*pointer })
    }
}

fn validate_atomic_u32_address(address: u64) -> Result<(), AddressSpaceError> {
    let end = address
        .checked_add(core::mem::size_of::<u32>() as u64)
        .ok_or(AddressSpaceError::AddressOverflow)?;
    if !address.is_multiple_of(core::mem::align_of::<AtomicU32>() as u64)
        || !(USER_SPACE_BASE..USER_SPACE_END_EXCLUSIVE).contains(&address)
        || end > USER_SPACE_END_EXCLUSIVE
    {
        return Err(AddressSpaceError::AddressOutOfRange);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{AddressSpaceError, USER_SPACE_BASE, validate_atomic_u32_address};

    #[test]
    fn atomic_user_u32_requires_aligned_complete_user_word() {
        assert_eq!(validate_atomic_u32_address(USER_SPACE_BASE), Ok(()));
        assert_eq!(
            validate_atomic_u32_address(USER_SPACE_BASE + 1),
            Err(AddressSpaceError::AddressOutOfRange)
        );
        assert_eq!(
            validate_atomic_u32_address(u64::MAX - 1),
            Err(AddressSpaceError::AddressOverflow)
        );
    }
}
