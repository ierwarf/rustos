//! Generic custody for page-aligned allocations in a service-owned aperture.
//!
//! The single persistent registry owns the fixed app-buffer aperture across
//! service publications. Callers must authenticate each exact service epoch.
//! This module does not install mappings.
//! Reuse requires provider retirement, all capability references to disappear,
//! and every admitted mapping to report completed unmap and TLB invalidation.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use spin::{Mutex, MutexGuard};

use driver_domain_protocol::DVM_APP_BUFFER_APERTURE_BYTES;

const PAGE_SIZE: u64 = 4096;
const REVOKED: usize = 1 << (usize::BITS - 1);
const MAP_COUNT: usize = REVOKED - 1;
/// `Allocation::billed_group` value while no Job is charged for the range.
const UNBILLED: u64 = u64::MAX;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BufferOwner {
    pub service_id: u64,
    pub generation: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BufferIdentity {
    pub owner: BufferOwner,
    pub allocation: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BufferRights(u8);

impl BufferRights {
    pub const READ: Self = Self(1);
    pub const WRITE: Self = Self(2);
    pub const TRANSFER: Self = Self(4);
    pub const ALL: Self = Self(7);

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BufferError {
    Invalid,
    NoSpace,
    NoMemory,
    Stale,
    Permission,
    Busy,
    Exhausted,
}

#[derive(Debug)]
struct Allocation {
    id: BufferIdentity,
    offset: u64,
    len: u64,
    // The high bit closes admission; the remaining bits count admitted VMAs.
    mappings: AtomicUsize,
    // Job charge group billed `Class::DeviceBuffer` for `len` bytes, or
    // `UNBILLED`. Set once by `BufferCapability::bill` for the original
    // holder (duplicates share this record, so they never double-charge) and
    // returned exactly once by `reclaim_after_scrub`.
    billed_group: AtomicU64,
}

impl Allocation {
    fn release_bill(&self) -> bool {
        let group = self.billed_group.swap(UNBILLED, Ordering::AcqRel);
        if group == UNBILLED {
            return false;
        }
        kernel_mm::api::kmem_charge::uncharge(
            kernel_mm::api::kmem_charge::Class::DeviceBuffer,
            group as usize,
            self.len,
        );
        true
    }
}

/// An attenuable capability, not a physical-address-bearing userspace token.
#[derive(Clone, Debug)]
pub struct BufferCapability {
    allocation: Arc<Allocation>,
    rights: BufferRights,
}

impl BufferCapability {
    pub fn identity(&self) -> BufferIdentity {
        self.allocation.id
    }
    pub fn len(&self) -> u64 {
        self.allocation.len
    }
    pub fn rights(&self) -> BufferRights {
        self.rights
    }
    /// Offset inside the app aperture. Callers add the aperture base only
    /// after the aperture has been verified (see `kernel-io-manager`).
    pub fn aperture_offset(&self) -> u64 {
        self.allocation.offset
    }
    /// Bills the whole allocation to `group` as `Class::DeviceBuffer`. Fails
    /// with `Exhausted` when the Job budget refuses and with `Busy` when the
    /// allocation is already billed (nothing is charged then). The charge is
    /// returned only by `reclaim_after_scrub`, or by `unbill` if the caller
    /// cannot finish installing the capability.
    pub fn bill(&self, group: usize) -> Result<(), BufferError> {
        use kernel_mm::api::kmem_charge::{try_charge, Class};
        if self
            .allocation
            .billed_group
            .compare_exchange(UNBILLED, group as u64, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(BufferError::Busy);
        }
        if !try_charge(Class::DeviceBuffer, group, self.allocation.len) {
            self.allocation.billed_group.store(UNBILLED, Ordering::Release);
            return Err(BufferError::Exhausted);
        }
        Ok(())
    }

    /// Refunds a charge made by `bill` when installation failed.
    pub fn unbill(&self) {
        self.allocation.release_bill();
    }

    /// Job charge group currently billed for this allocation, if any.
    pub fn billed_group(&self) -> Option<usize> {
        match self.allocation.billed_group.load(Ordering::Acquire) {
            UNBILLED => None,
            group => Some(group as usize),
        }
    }

    pub fn is_revoked(&self) -> bool {
        self.allocation.mappings.load(Ordering::Acquire) & REVOKED != 0
    }

    /// Includes the aperture registry's own reference and all descriptors,
    /// transfers, admitted mappings, and kernel provider custody references.
    pub fn custody_references(&self) -> usize {
        Arc::strong_count(&self.allocation)
    }

    pub fn attenuate(&self, rights: BufferRights) -> Result<Self, BufferError> {
        if rights.is_empty()
            || !self.rights.contains(rights)
            || !self.rights.contains(BufferRights::TRANSFER)
        {
            return Err(BufferError::Permission);
        }
        if self.allocation.mappings.load(Ordering::Acquire) & REVOKED != 0 {
            return Err(BufferError::Stale);
        }
        Ok(Self {
            allocation: Arc::clone(&self.allocation),
            rights,
        })
    }

    /// Admit one mapping (including a forked mapping). The returned hold must
    /// remain live until PTE removal and all relevant TLB shootdowns complete.
    pub fn admit_mapping(
        &self,
        offset: u64,
        len: u64,
        write: bool,
    ) -> Result<BufferMappingHold, BufferError> {
        if !self.rights.contains(BufferRights::READ)
            || (write && !self.rights.contains(BufferRights::WRITE))
        {
            return Err(BufferError::Permission);
        }
        if len == 0
            || offset % PAGE_SIZE != 0
            || len % PAGE_SIZE != 0
            || offset
                .checked_add(len)
                .filter(|end| *end <= self.len())
                .is_none()
        {
            return Err(BufferError::Invalid);
        }
        self.allocation
            .mappings
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |old| {
                if old & REVOKED != 0 || old & MAP_COUNT == MAP_COUNT {
                    None
                } else {
                    Some(old + 1)
                }
            })
            .map_err(|old| {
                if old & REVOKED != 0 {
                    BufferError::Stale
                } else {
                    BufferError::Exhausted
                }
            })?;
        Ok(BufferMappingHold {
            allocation: Arc::clone(&self.allocation),
        })
    }
}

/// One admitted mapping. Dropping it is the release, so every drop site must
/// run after PTE removal and the synchronous TLB shootdown: `munmap` drops it
/// from the post-unmap callback, and teardown/exec drop it inside the retired
/// execution payload, which the deferred owner frees only after the root has
/// been proved inactive and reclaimed. Forgotten (leaked) holds fail closed:
/// the range then stays quarantined and is never reused.
#[derive(Debug)]
pub struct BufferMappingHold {
    allocation: Arc<Allocation>,
}

impl BufferMappingHold {
    pub fn identity(&self) -> BufferIdentity {
        self.allocation.id
    }
    /// Absolute aperture offset of the first byte of the allocation.
    pub fn aperture_offset(&self) -> u64 {
        self.allocation.offset
    }
}

impl Drop for BufferMappingHold {
    fn drop(&mut self) {
        let previous = self.allocation.mappings.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous & MAP_COUNT != 0);
    }
}

#[derive(Debug)]
struct Slot {
    allocation: Arc<Allocation>,
    provider_retired: bool,
}

/// Persistent aperture-wide custody, shared across all provider epochs.
#[derive(Debug)]
pub struct BufferPool {
    bytes: u64,
    next_id: u64,
    slots: Vec<Slot>,
}

impl BufferPool {
    pub(crate) const fn new(bytes: u64) -> Self {
        Self {
            bytes,
            next_id: 1,
            slots: Vec::new(),
        }
    }

    /// `owner` must be obtained while holding the live service publication
    /// lease, not from an untrusted request or an endpoint object generation.
    pub fn allocate(
        &mut self,
        owner: BufferOwner,
        len: u64,
        rights: BufferRights,
    ) -> Result<BufferCapability, BufferError> {
        if owner.service_id == 0
            || owner.generation == 0
            || len == 0
            || len % PAGE_SIZE != 0
            || rights.is_empty()
            || !BufferRights::ALL.contains(rights)
        {
            return Err(BufferError::Invalid);
        }
        let next = self.next_id.checked_add(1).ok_or(BufferError::Exhausted)?;
        let mut offset = 0u64;
        // Slots remain ordered by offset; revoked slots remain occupied until
        // both sides have completed retirement.
        for slot in &self.slots {
            let end = offset.checked_add(len).ok_or(BufferError::NoSpace)?;
            if end <= slot.allocation.offset {
                break;
            }
            offset = slot
                .allocation
                .offset
                .checked_add(slot.allocation.len)
                .ok_or(BufferError::NoSpace)?;
        }
        if offset
            .checked_add(len)
            .filter(|end| *end <= self.bytes)
            .is_none()
        {
            return Err(BufferError::NoSpace);
        }
        self.slots
            .try_reserve(1)
            .map_err(|_| BufferError::NoMemory)?;
        let allocation = Arc::try_new(Allocation {
            id: BufferIdentity {
                owner,
                allocation: self.next_id,
            },
            offset,
            len,
            mappings: AtomicUsize::new(0),
            billed_group: AtomicU64::new(UNBILLED),
        })
        .map_err(|_| BufferError::NoMemory)?;
        let index = self
            .slots
            .partition_point(|slot| slot.allocation.offset < offset);
        self.slots.insert(
            index,
            Slot {
                allocation: Arc::clone(&allocation),
                provider_retired: false,
            },
        );
        self.next_id = next;
        Ok(BufferCapability { allocation, rights })
    }

    /// Only the trusted provider may use the offset to export a matching range.
    pub fn range(&self, id: BufferIdentity) -> Result<(u64, u64), BufferError> {
        let slot = self.find(id)?;
        if slot.allocation.mappings.load(Ordering::Acquire) & REVOKED != 0 {
            return Err(BufferError::Stale);
        }
        Ok((slot.allocation.offset, slot.allocation.len))
    }

    /// Retirement requests must still name the range after revocation closed
    /// admission; this never authorizes a new capability or mapping.
    pub fn range_including_revoked(&self, id: BufferIdentity) -> Result<(u64, u64), BufferError> {
        let slot = self.find(id)?;
        Ok((slot.allocation.offset, slot.allocation.len))
    }

    pub fn revoke(&mut self, id: BufferIdentity) -> Result<(), BufferError> {
        self.find(id)?
            .allocation
            .mappings
            .fetch_or(REVOKED, Ordering::AcqRel);
        Ok(())
    }

    /// Provider has destroyed exports and waited for all device DMA/fences.
    pub fn provider_retired(&mut self, id: BufferIdentity) -> Result<(), BufferError> {
        let slot = self
            .slots
            .iter_mut()
            .find(|slot| slot.allocation.id == id)
            .ok_or(BufferError::Stale)?;
        if slot.allocation.mappings.load(Ordering::Acquire) & REVOKED == 0 {
            return Err(BufferError::Busy);
        }
        slot.provider_retired = true;
        Ok(())
    }

    /// Once this succeeds, admission is permanently closed and the provider
    /// may scrub the returned range before returning it to the free pool.
    pub fn reclaimable_range(&self, id: BufferIdentity) -> Result<(u64, u64), BufferError> {
        let slot = self.find(id)?;
        if !slot.provider_retired
            || slot.allocation.mappings.load(Ordering::Acquire) != REVOKED
            || Arc::strong_count(&slot.allocation) != 1
        {
            return Err(BufferError::Busy);
        }
        Ok((slot.allocation.offset, slot.allocation.len))
    }

    /// First slot whose provider has retired and whose every capability and
    /// mapping reference is gone; the caller scrubs it outside this lock.
    pub fn next_reclaimable(&self) -> Option<BufferIdentity> {
        self.slots
            .iter()
            .find(|slot| {
                slot.provider_retired
                    && slot.allocation.mappings.load(Ordering::Acquire) == REVOKED
                    && Arc::strong_count(&slot.allocation) == 1
            })
            .map(|slot| slot.allocation.id)
    }

    /// Caller must scrub the entire range after `reclaimable_range` succeeds.
    pub fn reclaim_after_scrub(&mut self, id: BufferIdentity) -> Result<(), BufferError> {
        self.reclaimable_range(id)?;
        let index = self
            .slots
            .iter()
            .position(|slot| slot.allocation.id == id)
            .ok_or(BufferError::Stale)?;
        let slot = &self.slots[index];
        if !slot.provider_retired
            || slot.allocation.mappings.load(Ordering::Acquire) != REVOKED
            || Arc::strong_count(&slot.allocation) != 1
        {
            return Err(BufferError::Busy);
        }
        let slot = self.slots.remove(index);
        // Lock-free atomic refund; the backing range is scrubbed and free now.
        slot.allocation.release_bill();
        Ok(())
    }

    fn find(&self, id: BufferIdentity) -> Result<&Slot, BufferError> {
        self.slots
            .iter()
            .find(|slot| slot.allocation.id == id)
            .ok_or(BufferError::Stale)
    }
}

// This registry is never reset on service restart: unreclaimed slots from an
// old epoch must continue to exclude the corresponding aperture ranges.
static APP_BUFFER_POOL: Mutex<BufferPool> =
    Mutex::new(BufferPool::new(DVM_APP_BUFFER_APERTURE_BYTES));

/// Kernel-internal custody lock. Do not hold across IPC, user copy, or scrub.
/// Authorization and the publication lease must be held by the caller.
pub fn app_buffer_pool() -> MutexGuard<'static, BufferPool> {
    APP_BUFFER_POOL.lock()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn owner() -> BufferOwner {
        BufferOwner {
            service_id: 4,
            generation: 9,
        }
    }
    fn pool() -> BufferPool {
        BufferPool::new(3 * PAGE_SIZE)
    }

    #[test]
    fn ranges_are_disjoint_and_capacity_is_bounded() {
        let mut p = pool();
        let a = p.allocate(owner(), PAGE_SIZE, BufferRights::ALL).unwrap();
        let b = p
            .allocate(owner(), 2 * PAGE_SIZE, BufferRights::ALL)
            .unwrap();
        assert_eq!(p.range(a.identity()), Ok((0, PAGE_SIZE)));
        assert_eq!(p.range(b.identity()), Ok((PAGE_SIZE, 2 * PAGE_SIZE)));
        assert_eq!(
            p.allocate(owner(), PAGE_SIZE, BufferRights::ALL)
                .unwrap_err(),
            BufferError::NoSpace
        );
        assert_eq!(
            p.allocate(owner(), 1, BufferRights::ALL).unwrap_err(),
            BufferError::Invalid
        );
    }

    #[test]
    fn revoke_requires_provider_tlb_and_capability_custody_before_reuse() {
        let mut p = pool();
        let a = p
            .allocate(owner(), 3 * PAGE_SIZE, BufferRights::ALL)
            .unwrap();
        let id = a.identity();
        let hold = a.admit_mapping(0, PAGE_SIZE, true).unwrap();
        assert_eq!(p.provider_retired(id), Err(BufferError::Busy));
        p.revoke(id).unwrap();
        assert_eq!(
            a.admit_mapping(0, PAGE_SIZE, false).unwrap_err(),
            BufferError::Stale
        );
        p.provider_retired(id).unwrap();
        assert_eq!(p.reclaim_after_scrub(id), Err(BufferError::Busy));
        drop(a);
        assert_eq!(p.reclaim_after_scrub(id), Err(BufferError::Busy));
        drop(hold);
        p.reclaim_after_scrub(id).unwrap();
        let b = p
            .allocate(owner(), 3 * PAGE_SIZE, BufferRights::ALL)
            .unwrap();
        assert_ne!(id, b.identity());
        assert_eq!(p.revoke(id), Err(BufferError::Stale));
    }

    #[test]
    fn attenuation_and_mapping_ranges_are_checked() {
        let mut p = pool();
        let a = p.allocate(owner(), PAGE_SIZE, BufferRights::ALL).unwrap();
        let read = a.attenuate(BufferRights::READ).unwrap();
        assert_eq!(
            read.attenuate(BufferRights::READ).unwrap_err(),
            BufferError::Permission
        );
        assert_eq!(
            read.admit_mapping(0, PAGE_SIZE, true).unwrap_err(),
            BufferError::Permission
        );
        assert_eq!(
            a.admit_mapping(PAGE_SIZE, PAGE_SIZE, false).unwrap_err(),
            BufferError::Invalid
        );
        assert_eq!(
            a.admit_mapping(u64::MAX, PAGE_SIZE, false).unwrap_err(),
            BufferError::Invalid
        );
        drop(a.admit_mapping(0, PAGE_SIZE, false).unwrap());
    }

    #[test]
    fn leaked_mapping_hold_keeps_range_quarantined() {
        let mut p = pool();
        let a = p.allocate(owner(), PAGE_SIZE, BufferRights::ALL).unwrap();
        let id = a.identity();
        core::mem::forget(a.admit_mapping(0, PAGE_SIZE, false).unwrap());
        p.revoke(id).unwrap();
        p.provider_retired(id).unwrap();
        drop(a);
        assert_eq!(p.reclaim_after_scrub(id), Err(BufferError::Busy));
    }

    #[test]
    fn next_reclaimable_waits_for_every_custody_condition() {
        let mut p = pool();
        let a = p.allocate(owner(), PAGE_SIZE, BufferRights::ALL).unwrap();
        let id = a.identity();
        assert_eq!(p.next_reclaimable(), None);
        p.revoke(id).unwrap();
        assert_eq!(p.next_reclaimable(), None);
        p.provider_retired(id).unwrap();
        assert_eq!(p.next_reclaimable(), None);
        drop(a);
        assert_eq!(p.next_reclaimable(), Some(id));
    }

    #[test]
    fn mapping_after_revoke_is_stale_even_for_attenuated_copies() {
        let mut p = pool();
        let a = p.allocate(owner(), PAGE_SIZE, BufferRights::ALL).unwrap();
        let read = a.attenuate(BufferRights::READ.union(BufferRights::TRANSFER)).unwrap();
        p.revoke(a.identity()).unwrap();
        assert!(read.is_revoked());
        assert_eq!(read.admit_mapping(0, PAGE_SIZE, false).unwrap_err(), BufferError::Stale);
    }

    #[test]
    fn exhaustion_is_reported_and_freed_space_is_reusable_only_after_reclaim() {
        let mut p = pool();
        let whole = p.bytes;
        let a = p.allocate(owner(), whole, BufferRights::ALL).unwrap();
        assert_eq!(
            p.allocate(owner(), PAGE_SIZE, BufferRights::ALL).unwrap_err(),
            BufferError::NoSpace
        );
        let id = a.identity();
        p.revoke(id).unwrap();
        assert_eq!(
            p.allocate(owner(), PAGE_SIZE, BufferRights::ALL).unwrap_err(),
            BufferError::NoSpace
        );
        p.provider_retired(id).unwrap();
        drop(a);
        p.reclaim_after_scrub(id).unwrap();
        p.allocate(owner(), PAGE_SIZE, BufferRights::ALL).unwrap();
    }
    #[test]
    fn provider_republication_cannot_overlap_unretired_old_epoch() {
        let mut p = pool();
        let a = p
            .allocate(owner(), 2 * PAGE_SIZE, BufferRights::ALL)
            .unwrap();
        let replacement = BufferOwner {
            generation: 10,
            ..owner()
        };
        let b = p
            .allocate(replacement, PAGE_SIZE, BufferRights::ALL)
            .unwrap();
        assert_eq!(p.range(a.identity()), Ok((0, 2 * PAGE_SIZE)));
        assert_eq!(p.range(b.identity()), Ok((2 * PAGE_SIZE, PAGE_SIZE)));
        assert_eq!(
            p.allocate(replacement, PAGE_SIZE, BufferRights::ALL)
                .unwrap_err(),
            BufferError::NoSpace
        );
        let id = a.identity();
        p.revoke(id).unwrap();
        p.provider_retired(id).unwrap();
        drop(a);
        p.reclaim_after_scrub(id).unwrap();
        let c = p
            .allocate(replacement, PAGE_SIZE, BufferRights::ALL)
            .unwrap();
        assert_eq!(p.range(c.identity()), Ok((0, PAGE_SIZE)));
        assert_ne!(id, c.identity());
    }

    #[test]
    fn job_charge_refuses_at_limit_and_reclaim_uncharges_once() {
        use kernel_mm::api::kmem_charge::{set_limit, usage, Class};
        const G: usize = 41;
        set_limit(Class::DeviceBuffer, G, 2 * PAGE_SIZE);
        let mut p = pool();
        let a = p.allocate(owner(), 2 * PAGE_SIZE, BufferRights::ALL).unwrap();
        let b = p.allocate(owner(), PAGE_SIZE, BufferRights::ALL).unwrap();
        a.bill(G).unwrap();
        assert_eq!(usage(Class::DeviceBuffer, G), 2 * PAGE_SIZE);
        assert_eq!(b.bill(G), Err(BufferError::Exhausted), "over-limit refuses");
        assert_eq!(b.billed_group(), None);
        assert_eq!(usage(Class::DeviceBuffer, G), 2 * PAGE_SIZE, "refusal charges nothing");
        assert_eq!(a.bill(G), Err(BufferError::Busy), "already billed: no double charge");
        // Duplicates and attenuated copies share the one charge.
        let dup = a.clone();
        let att = a.attenuate(BufferRights::READ).unwrap();
        assert_eq!(usage(Class::DeviceBuffer, G), 2 * PAGE_SIZE);
        let id = a.identity();
        p.revoke(id).unwrap();
        assert_eq!(usage(Class::DeviceBuffer, G), 2 * PAGE_SIZE, "revoke does not uncharge");
        p.provider_retired(id).unwrap();
        drop((a, dup));
        assert_eq!(p.reclaim_after_scrub(id), Err(BufferError::Busy));
        assert_eq!(usage(Class::DeviceBuffer, G), 2 * PAGE_SIZE, "refused reclaim keeps the charge");
        drop(att);
        p.reclaim_after_scrub(id).unwrap();
        assert_eq!(usage(Class::DeviceBuffer, G), 0);
        assert_eq!(p.reclaim_after_scrub(id), Err(BufferError::Stale));
        assert_eq!(usage(Class::DeviceBuffer, G), 0, "no second refund");
        b.bill(G).unwrap();
        b.unbill();
        assert_eq!(usage(Class::DeviceBuffer, G), 0, "failed install refunds");
        b.unbill();
        assert_eq!(usage(Class::DeviceBuffer, G), 0, "refund is idempotent");
        set_limit(Class::DeviceBuffer, G, kernel_mm::api::charge::UNLIMITED);
    }
}
