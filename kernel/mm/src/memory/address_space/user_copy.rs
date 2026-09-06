//! Validated user-copy proofs and the exact-span admission behind them.
//!
//! - **Owner:** `kernel-mm` owns user-range admission and the byte movers; the
//!   parent module owns page-table mutation and region lifetime.
//! - **Boundary:** Every user pointer, length, and direction arriving here is
//!   untrusted until complete-span admission has run.
//! - **Lifecycle:** Admit the whole span, carry the first page's translation
//!   into the proof, then move all bytes or none.
//! - **Concurrency:** The caller holds an exact process/MM generation for the
//!   life of a proof. An explicitly supplied COW resolver may split a leaf
//!   before proof construction; the resulting translation is read afresh.
//! - **Failure:** Noncanonical, overflowing, unmapped, and permission errors
//!   return before any byte moves.
//! - **Forbidden:** No raw slice over user memory before admission, and no
//!   proof outliving the address-space borrow that produced it.
//! - **Evidence:** `user-memory-access`; `docs/benchmarks/README.md` for the
//!   single-walk cost.
//!
//! This is the first boundary named by `formal/rust-large-files.tsv`'s split
//! plan for the parent module.

use core::cmp::min;
use core::ptr;

use x86_64::PhysAddr;
use x86_64::VirtAddr;

use super::{
    AddressSpaceError, PAGE_4KIB, PAGE_4KIB_U64, ProcessAddressSpace, UserBufferAccess, align_down,
    align_up, higher_half_ptr, is_user_addr, validate_user_page_access,
};

/// Proof that one exact user range was admitted for reading against this
/// address-space snapshot.  Keeping the proof tied to `&ProcessAddressSpace`
/// lets a caller copy without walking the same page tables a second time.
///
/// `first_phys` is what makes that claim true rather than aspirational.
/// Admission already resolved every page of the span; carrying the first one's
/// translation forward means a copy that stays inside one page -- which is
/// every fixed-layout IPC request, reply, and typed struct in the kernel --
/// performs no page-table walk of its own. The caller's retained MM generation
/// is what makes the carried translation exact: the mapping cannot change
/// between admission and copy without invalidating the bind that produced this
/// proof.
#[must_use]
pub struct ValidatedUserRead<'a> {
    address_space: &'a ProcessAddressSpace,
    start: VirtAddr,
    byte_len: usize,
    first_phys: Option<PhysAddr>,
}

impl ValidatedUserRead<'_> {
    pub fn copy_into(self, dest: &mut [u8]) -> Result<(), AddressSpaceError> {
        assert_eq!(
            dest.len(),
            self.byte_len,
            "validated user-read proof used with a different byte length"
        );
        self.address_space
            .read_user_bytes_from(self.start, dest, self.first_phys)
    }
}

/// Proof that one exact user range was admitted for writing against this
/// address-space snapshot. See [`ValidatedUserRead`] for why the proof carries
/// the first page's translation.
#[must_use]
pub struct ValidatedUserWrite<'a> {
    address_space: &'a ProcessAddressSpace,
    start: VirtAddr,
    byte_len: usize,
    first_phys: Option<PhysAddr>,
}

impl ValidatedUserWrite<'_> {
    pub fn copy_from(self, data: &[u8]) -> Result<(), AddressSpaceError> {
        assert_eq!(
            data.len(),
            self.byte_len,
            "validated user-write proof used with a different byte length"
        );
        self.address_space
            .write_user_bytes_from(self.start, data, self.first_phys)
    }
}

impl ProcessAddressSpace {
    pub fn copy_into_user(&self, start: VirtAddr, data: &[u8]) -> Result<(), AddressSpaceError> {
        let first_phys =
            self.validate_user_buffer_access(start, data.len(), UserBufferAccess::Write)?;
        self.write_user_bytes_from(start, data, first_phys)
    }

    pub fn validate_user_write_buffer(
        &self,
        start: VirtAddr,
        byte_len: usize,
    ) -> Result<(), AddressSpaceError> {
        self.validate_user_write(start, byte_len).map(drop)
    }

    pub fn validate_user_write(
        &self,
        start: VirtAddr,
        byte_len: usize,
    ) -> Result<ValidatedUserWrite<'_>, AddressSpaceError> {
        let first_phys =
            self.validate_user_buffer_access(start, byte_len, UserBufferAccess::Write)?;
        Ok(ValidatedUserWrite {
            address_space: self,
            start,
            byte_len,
            first_phys,
        })
    }

    /// Admits copyout with an owner-supplied resolver for readonly COW leaves.
    /// The resolver must retain exact process/MM identity and logical VMA
    /// write authority. Its success is not proof: the PTE is translated and
    /// checked again before the new physical address enters the copy proof.
    pub fn validate_user_write_resolving_cow(
        &self,
        start: VirtAddr,
        byte_len: usize,
        resolve: impl FnMut(VirtAddr) -> Result<(), AddressSpaceError>,
    ) -> Result<ValidatedUserWrite<'_>, AddressSpaceError> {
        let first_phys = self.validate_user_buffer_access_resolving_cow(
            start,
            byte_len,
            UserBufferAccess::Write,
            resolve,
        )?;
        Ok(ValidatedUserWrite {
            address_space: self,
            start,
            byte_len,
            first_phys,
        })
    }

    pub fn validate_user_read_buffer(
        &self,
        start: VirtAddr,
        byte_len: usize,
    ) -> Result<(), AddressSpaceError> {
        self.validate_user_read(start, byte_len).map(drop)
    }

    pub fn validate_user_read(
        &self,
        start: VirtAddr,
        byte_len: usize,
    ) -> Result<ValidatedUserRead<'_>, AddressSpaceError> {
        let first_phys =
            self.validate_user_buffer_access(start, byte_len, UserBufferAccess::Read)?;
        Ok(ValidatedUserRead {
            address_space: self,
            start,
            byte_len,
            first_phys,
        })
    }

    pub fn initialize_user_bytes(
        &self,
        start: VirtAddr,
        data: &[u8],
    ) -> Result<(), AddressSpaceError> {
        // Loader-time initialization has no admission to carry, so it walks.
        self.write_user_bytes_from(start, data, None)
    }

    /// The write direction of [`Self::read_user_bytes_from`], with the same
    /// requirement on `first_phys`.
    fn write_user_bytes_from(
        &self,
        start: VirtAddr,
        data: &[u8],
        first_phys: Option<PhysAddr>,
    ) -> Result<(), AddressSpaceError> {
        if data.is_empty() {
            return Ok(());
        }

        let mut cursor = start.as_u64();
        let mut written = 0usize;
        let mut carried = first_phys;

        while written < data.len() {
            let virt = VirtAddr::new(cursor);
            let phys = match carried.take() {
                Some(phys) => phys,
                None => self
                    .translate_user(virt)
                    .ok_or(AddressSpaceError::NotMapped)?,
            };
            let page_offset = (cursor as usize) & (PAGE_4KIB - 1);
            let chunk = min(PAGE_4KIB - page_offset, data.len() - written);

            unsafe {
                ptr::copy_nonoverlapping(data.as_ptr().add(written), higher_half_ptr(phys), chunk);
            }

            cursor = cursor
                .checked_add(chunk as u64)
                .ok_or(AddressSpaceError::AddressOverflow)?;
            written += chunk;
        }

        Ok(())
    }

    pub fn copy_from_user(
        &self,
        start: VirtAddr,
        dest: &mut [u8],
    ) -> Result<(), AddressSpaceError> {
        let first_phys =
            self.validate_user_buffer_access(start, dest.len(), UserBufferAccess::Read)?;
        self.read_user_bytes_from(start, dest, first_phys)
    }

    pub fn visit_user_read_spans(
        &self,
        start: VirtAddr,
        byte_len: usize,
        mut visit: impl FnMut(*const u8, usize) -> Result<(), AddressSpaceError>,
    ) -> Result<(), AddressSpaceError> {
        if byte_len == 0 {
            return Ok(());
        }

        let start_addr = start.as_u64();
        if !is_user_addr(start_addr) {
            return Err(AddressSpaceError::AddressOutOfRange);
        }

        let last_addr = start_addr
            .checked_add(byte_len as u64 - 1)
            .ok_or(AddressSpaceError::AddressOverflow)?;
        if !is_user_addr(last_addr) {
            return Err(AddressSpaceError::AddressOutOfRange);
        }

        let end_exclusive = last_addr
            .checked_add(1)
            .ok_or(AddressSpaceError::AddressOverflow)?;
        let mut cursor = start_addr;

        while cursor < end_exclusive {
            let virt = VirtAddr::new(cursor);
            let (phys, flags) = self
                .translate_user_with_flags(virt)
                .ok_or(AddressSpaceError::NotMapped)?;
            validate_user_page_access(flags, UserBufferAccess::Read)?;

            let page_offset = (cursor as usize) & (PAGE_4KIB - 1);
            let chunk = min(PAGE_4KIB - page_offset, (end_exclusive - cursor) as usize);
            visit(higher_half_ptr(phys) as *const u8, chunk)?;

            cursor = cursor
                .checked_add(chunk as u64)
                .ok_or(AddressSpaceError::AddressOverflow)?;
        }

        Ok(())
    }

    /// Moves admitted bytes out of user memory, reusing `first_phys` for the
    /// first page instead of translating an address admission already resolved.
    ///
    /// `first_phys` must be `start`'s own translation under the same retained
    /// MM generation that admitted the span. `None` re-walks, which is what an
    /// unadmitted internal caller gets.
    fn read_user_bytes_from(
        &self,
        start: VirtAddr,
        dest: &mut [u8],
        first_phys: Option<PhysAddr>,
    ) -> Result<(), AddressSpaceError> {
        if dest.is_empty() {
            return Ok(());
        }

        let mut cursor = start.as_u64();
        let mut copied = 0usize;
        let mut carried = first_phys;

        while copied < dest.len() {
            let virt = VirtAddr::new(cursor);
            let phys = match carried.take() {
                Some(phys) => phys,
                None => self
                    .translate_user(virt)
                    .ok_or(AddressSpaceError::NotMapped)?,
            };
            let page_offset = (cursor as usize) & (PAGE_4KIB - 1);
            let chunk = min(PAGE_4KIB - page_offset, dest.len() - copied);

            unsafe {
                ptr::copy_nonoverlapping(
                    higher_half_ptr(phys) as *const u8,
                    dest.as_mut_ptr().add(copied),
                    chunk,
                );
            }

            cursor = cursor
                .checked_add(chunk as u64)
                .ok_or(AddressSpaceError::AddressOverflow)?;
            copied += chunk;
        }

        Ok(())
    }

    /// Admits every page of the span and returns `start`'s own translation.
    ///
    /// The returned address is the exact physical address of `start`, offset
    /// included, so a copy that stays inside the first page never re-walks.
    /// An empty span admits nothing and translates nothing.
    fn validate_user_buffer_access(
        &self,
        start: VirtAddr,
        byte_len: usize,
        access: UserBufferAccess,
    ) -> Result<Option<PhysAddr>, AddressSpaceError> {
        self.validate_user_buffer_access_resolving_cow(start, byte_len, access, |_| {
            Err(AddressSpaceError::ProtectionViolation)
        })
    }

    fn validate_user_buffer_access_resolving_cow(
        &self,
        start: VirtAddr,
        byte_len: usize,
        access: UserBufferAccess,
        mut resolve: impl FnMut(VirtAddr) -> Result<(), AddressSpaceError>,
    ) -> Result<Option<PhysAddr>, AddressSpaceError> {
        if byte_len == 0 {
            return Ok(None);
        }

        let start_addr = start.as_u64();
        if !is_user_addr(start_addr) {
            return Err(AddressSpaceError::AddressOutOfRange);
        }

        let last_addr = start_addr
            .checked_add(byte_len as u64 - 1)
            .ok_or(AddressSpaceError::AddressOverflow)?;
        if !is_user_addr(last_addr) {
            return Err(AddressSpaceError::AddressOutOfRange);
        }

        let mut cursor = align_down(start_addr, PAGE_4KIB_U64);
        let end_exclusive = align_up(
            last_addr
                .checked_add(1)
                .ok_or(AddressSpaceError::AddressOverflow)?,
            PAGE_4KIB_U64,
        )
        .ok_or(AddressSpaceError::AddressOverflow)?;

        let mut first_page_phys = None;
        while cursor < end_exclusive {
            let (mut phys, mut flags) = self
                .translate_user_with_flags(VirtAddr::new(cursor))
                .ok_or(AddressSpaceError::NotMapped)?;
            if matches!(access, UserBufferAccess::Write)
                && flags.contains(super::COW_USER_LEAF)
                && !flags.contains(x86_64::structures::paging::PageTableFlags::WRITABLE)
            {
                resolve(VirtAddr::new(cursor))?;
                (phys, flags) = self
                    .translate_user_with_flags(VirtAddr::new(cursor))
                    .ok_or(AddressSpaceError::NotMapped)?;
            }
            validate_user_page_access(flags, access)?;
            if first_page_phys.is_none() {
                first_page_phys = Some(phys);
            }
            cursor = cursor
                .checked_add(PAGE_4KIB_U64)
                .ok_or(AddressSpaceError::AddressOverflow)?;
        }

        // The loop translated the page-aligned base, so re-apply `start`'s own
        // offset inside that page rather than translating a second address.
        let page_offset = start_addr & (PAGE_4KIB_U64 - 1);
        Ok(first_page_phys.map(|phys| PhysAddr::new(phys.as_u64().saturating_add(page_offset))))
    }
}
