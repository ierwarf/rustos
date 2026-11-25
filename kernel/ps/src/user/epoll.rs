//! Kernel-local epoll open-description identity, descriptor ownership, and
//! interest/report/reverse-index registry.
//!
//! - **Owner:** `kernel-ps` owns fd-table reference lifetime and the epoll
//!   interest/report/reverse-index registry; provider services own per-object
//!   readiness generations.
//! - **Boundary:** Tokens are kernel-minted; only published descriptors count.
//! - **Lifecycle:** Create with one ref and one registry slot; dup/fork/transfer
//!   retain; final close reaches zero once and retires the registry slot.
//! - **Concurrency:** Transient `Arc` snapshots never affect descriptor refs;
//!   atomic retain rejects zero and release panics on ownership underflow.
//!   `EPOLL_REGISTRY` is protected by `LockClass::KernelEpollRegistry` and is
//!   never acquired while holding `LocalFdObject` or `ProcessState`, nor held
//!   across `wake_local_fd_object` or provider IPC.
//! - **Failure:** Provider delay cannot fail ordinary descriptor duplication;
//!   object and per-epoll interest caps fail explicitly with `EpollError::Capacity`.
//! - **Accounting:** each instance owns a `PageLease` on the creating Job's charge
//!   group: 1 object page plus 1 page per 16 interests, resized on ctl ADD/DEL and
//!   purge, refunded when `retire` drops the instance. A refused charge is
//!   `Capacity` (`ENOSPC` on ADD, `EMFILE` on create) and changes nothing.
//! - **Forbidden:** No vfsd call per epoll op, zero resurrection, or token aliasing.
//! - **Evidence:** `userspace-wait-set`, requirements REQ-VFS-016/017.
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::user::page_lease::PageLease;
use nucleus_core::util::lockdep::{LockClass, TrackedSpinLock};
use rustos_user_abi::linux as linux_abi;
use rustos_user_abi::performance::{WAITSET_MAX_EPOLL_OBJECTS, WAITSET_MAX_GLOBAL_INTERESTS};
use rustos_user_abi::syscall::WAITSET_MAX_INTERESTS;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum EpollError {
    Busy,
    Capacity,
    Exists,
    InvalidArgument,
    NotFound,
}

/// Identity of one registered epoll interest. Excludes `provider_epoch` so a
/// provider restart updates the epoch on `EPOLL_CTL_MOD` without duplicating
/// registration identity.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct EpollInterestKey {
    pub target_fd: u64,
    pub provider: u16,
    pub object_id: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EpollInterestRecord {
    pub key: EpollInterestKey,
    pub provider_epoch: u64,
    pub events: u32,
    pub data: u64,
}

/// Volatile report state for edge-triggered (`EPOLLET`) and one-shot
/// (`EPOLLONESHOT`) interests.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EpollReportState {
    /// Provider generation of the last report this interest consumed.
    pub edge_generation: u64,
    /// A one-shot interest already reported; `EPOLL_CTL_MOD` re-arms it.
    pub disabled: bool,
}

/// Linux `epoll_ctl` accepts any mask except that `EPOLLEXCLUSIVE` may only be
/// combined with `EPOLLEXCLUSIVE_OK_BITS`.
pub const fn epoll_event_mask_valid(events: u32) -> bool {
    events & linux_abi::EPOLLEXCLUSIVE == 0 || events & !linux_abi::EPOLLEXCLUSIVE_OK_BITS == 0
}

/// Interests billed per resident page (map node + report slot + reverse-index
/// entry are about 250 bytes, so 16 fit in 4 KiB).
const EPOLL_INTERESTS_PER_PAGE: usize = 16;

/// Pages an instance with `interests` registered interests occupies: one for
/// the object plus the interest sets.
fn epoll_pages(interests: usize) -> u64 {
    1 + interests.div_ceil(EPOLL_INTERESTS_PER_PAGE) as u64
}

#[derive(Debug)]
struct EpollInstance {
    /// Object and interest-set pages billed to the creating Job; refunded
    /// exactly once when the instance is retired (the lease drops with it).
    lease: PageLease,
    interests: BTreeMap<EpollInterestKey, EpollInterestRecord>,
    /// Entries exist only for interests that reported; removing or modifying
    /// an interest removes its entry, so this map never outgrows `interests`.
    report: BTreeMap<EpollInterestKey, EpollReportState>,
    cursor: usize,
    generation: u64,
}

/// Kernel-local registry of all live epoll instances, their interests, report
/// state, and `(provider, object_id)` reverse purge index.
#[derive(Debug, Default)]
pub struct EpollRegistry {
    epolls: BTreeMap<u64, EpollInstance>,
    object_interests: BTreeMap<(u16, u64), BTreeSet<(u64, EpollInterestKey)>>,
}

impl EpollRegistry {
    pub const fn new() -> Self {
        Self {
            epolls: BTreeMap::new(),
            object_interests: BTreeMap::new(),
        }
    }

    fn object_key(key: EpollInterestKey) -> (u16, u64) {
        (key.provider, key.object_id)
    }

    fn insert_object_interest(&mut self, token: u64, key: EpollInterestKey) {
        let members = self
            .object_interests
            .entry(Self::object_key(key))
            .or_default();
        let inserted = members.insert((token, key));
        assert!(inserted, "epoll reverse index duplicated a live interest");
        assert!(
            members.len() <= WAITSET_MAX_GLOBAL_INTERESTS,
            "epoll reverse index exceeded global interest capacity"
        );
    }

    fn remove_object_interest(&mut self, token: u64, key: EpollInterestKey) {
        let object = Self::object_key(key);
        let remove_bucket = {
            let members = self
                .object_interests
                .get_mut(&object)
                .expect("epoll reverse index lacks a live object");
            assert!(
                members.remove(&(token, key)),
                "epoll reverse index lacks a live interest"
            );
            members.is_empty()
        };
        if remove_bucket {
            self.object_interests.remove(&object);
        }
    }

    /// Registers a new instance billed to `charge_group` (`None` bills
    /// nothing); a refused charge is `Capacity` and changes nothing.
    pub fn create(&mut self, token: u64, charge_group: Option<usize>) -> Result<u64, EpollError> {
        if token == 0 || self.epolls.contains_key(&token) {
            return Err(EpollError::Exists);
        }
        if self.epolls.len() >= WAITSET_MAX_EPOLL_OBJECTS {
            return Err(EpollError::Capacity);
        }
        let lease = PageLease::acquire(charge_group, epoll_pages(0)).ok_or(EpollError::Capacity)?;
        self.epolls.insert(
            token,
            EpollInstance {
                lease,
                interests: BTreeMap::new(),
                report: BTreeMap::new(),
                cursor: 0,
                generation: 1,
            },
        );
        Ok(1)
    }

    pub fn ctl(
        &mut self,
        token: u64,
        op: u64,
        record: EpollInterestRecord,
    ) -> Result<u64, EpollError> {
        match op {
            linux_abi::EPOLL_CTL_ADD => {
                let epoll = self.epolls.get_mut(&token).ok_or(EpollError::NotFound)?;
                if !epoll_event_mask_valid(record.events) {
                    return Err(EpollError::InvalidArgument);
                }
                if epoll.interests.contains_key(&record.key) {
                    return Err(EpollError::Exists);
                }
                if epoll.interests.len() >= WAITSET_MAX_INTERESTS {
                    return Err(EpollError::Capacity);
                }
                if !epoll.lease.resize(epoll_pages(epoll.interests.len() + 1)) {
                    return Err(EpollError::Capacity);
                }
                epoll.interests.insert(record.key, record);
                epoll.generation = epoll.generation.saturating_add(1);
                let generation = epoll.generation;
                self.insert_object_interest(token, record.key);
                Ok(generation)
            }
            linux_abi::EPOLL_CTL_MOD => {
                let epoll = self.epolls.get_mut(&token).ok_or(EpollError::NotFound)?;
                let Some(existing) = epoll.interests.get(&record.key) else {
                    return Err(EpollError::NotFound);
                };
                // Linux: an interest added with EPOLLEXCLUSIVE can never be
                // modified, and EPOLLEXCLUSIVE can never be introduced by a
                // modification.
                if existing.events & linux_abi::EPOLLEXCLUSIVE != 0
                    || record.events & linux_abi::EPOLLEXCLUSIVE != 0
                    || !epoll_event_mask_valid(record.events)
                {
                    return Err(EpollError::InvalidArgument);
                }
                // Modification re-arms: clear one-shot and edge history so the
                // current readiness is reported again.
                epoll.report.remove(&record.key);
                epoll.interests.insert(record.key, record);
                epoll.generation = epoll.generation.saturating_add(1);
                Ok(epoll.generation)
            }
            linux_abi::EPOLL_CTL_DEL => {
                let epoll = self.epolls.get_mut(&token).ok_or(EpollError::NotFound)?;
                let interest = epoll
                    .interests
                    .remove(&record.key)
                    .ok_or(EpollError::NotFound)?;
                epoll.report.remove(&record.key);
                let shrunk = epoll.lease.resize(epoll_pages(epoll.interests.len()));
                debug_assert!(shrunk, "shrinking a lease never fails");
                if epoll.interests.is_empty() {
                    epoll.cursor = 0;
                } else {
                    epoll.cursor %= epoll.interests.len();
                }
                epoll.generation = epoll.generation.saturating_add(1);
                let generation = epoll.generation;
                self.remove_object_interest(token, interest.key);
                Ok(generation)
            }
            _ => Err(EpollError::InvalidArgument),
        }
    }

    pub fn snapshot(
        &mut self,
        token: u64,
        maxevents: usize,
    ) -> Result<(u64, Vec<(EpollInterestRecord, EpollReportState)>), EpollError> {
        let epoll = self.epolls.get_mut(&token).ok_or(EpollError::NotFound)?;
        let count = epoll.interests.len();
        let start = epoll.cursor.min(count.saturating_sub(1));
        let mut entries = Vec::with_capacity(count.min(maxevents));
        for interest in epoll
            .interests
            .values()
            .skip(start)
            .chain(epoll.interests.values().take(start))
            .take(maxevents)
        {
            let report = epoll.report.get(&interest.key).copied().unwrap_or_default();
            entries.push((*interest, report));
        }
        if count != 0 {
            epoll.cursor = (start + 1) % count;
        }
        Ok((epoll.generation, entries))
    }

    /// Consumes one report for an edge-triggered or one-shot interest.
    ///
    /// Returns `true` only for the single caller that owns the report: a
    /// disabled one-shot interest, a stale generation (the edge was already
    /// reported), or a vanished interest returns `false`. Level-triggered
    /// interests always return `true` and keep no state.
    pub fn claim(
        &mut self,
        token: u64,
        key: EpollInterestKey,
        generation: u64,
    ) -> Result<bool, EpollError> {
        let epoll = self.epolls.get_mut(&token).ok_or(EpollError::NotFound)?;
        let Some(interest) = epoll.interests.get(&key) else {
            return Ok(false);
        };
        let edge = interest.events & linux_abi::EPOLLET != 0;
        let oneshot = interest.events & linux_abi::EPOLLONESHOT != 0;
        if !edge && !oneshot {
            return Ok(true);
        }
        let state = epoll.report.entry(key).or_default();
        if state.disabled || (edge && generation <= state.edge_generation) {
            return Ok(false);
        }
        state.edge_generation = state.edge_generation.max(generation);
        state.disabled = oneshot;
        Ok(true)
    }

    pub fn retire(&mut self, token: u64) -> Option<u64> {
        let epoll = self.epolls.remove(&token)?;
        for interest in epoll.interests.into_values() {
            self.remove_object_interest(token, interest.key);
        }
        Some(epoll.generation.saturating_add(1))
    }

    /// Removes exactly one provider object from every live epoll instance via
    /// the reverse index and returns `(epoll_token, new_generation)` pairs to
    /// wake after releasing the registry lock.
    pub fn purge_object(&mut self, provider: u16, object_id: u64) -> Vec<(u64, u64)> {
        let Some(members) = self.object_interests.remove(&(provider, object_id)) else {
            return Vec::new();
        };
        assert!(
            members.len() <= WAITSET_MAX_GLOBAL_INTERESTS,
            "epoll purge exceeded global interest capacity"
        );
        let mut wakes: Vec<(u64, u64)> = Vec::new();
        for (epoll_token, key) in members {
            let epoll = self
                .epolls
                .get_mut(&epoll_token)
                .expect("epoll reverse index references a retired epoll");
            assert!(
                epoll.interests.remove(&key).is_some(),
                "epoll reverse index references a missing interest"
            );
            epoll.report.remove(&key);
            let shrunk = epoll.lease.resize(epoll_pages(epoll.interests.len()));
            debug_assert!(shrunk, "shrinking a lease never fails");
            if epoll.interests.is_empty() {
                epoll.cursor = 0;
            } else {
                epoll.cursor %= epoll.interests.len();
            }
            epoll.generation = epoll.generation.saturating_add(1);
            let new_generation = epoll.generation;
            // `BTreeSet<(u64, EpollInterestKey)>` orders by `epoll_token` first,
            // so collapse multiple purged fds on the same epoll into one wake.
            if let Some(last) = wakes.last_mut()
                && last.0 == epoll_token
            {
                last.1 = new_generation;
            } else {
                wakes.push((epoll_token, new_generation));
            }
        }
        wakes
    }
}

static EPOLL_REGISTRY: TrackedSpinLock<EpollRegistry, { LockClass::KernelEpollRegistry as u8 }> =
    TrackedSpinLock::new(EpollRegistry::new());

pub fn epoll_create(token: u64, charge_group: Option<usize>) -> Result<u64, EpollError> {
    EPOLL_REGISTRY.lock().create(token, charge_group)
}

pub fn epoll_ctl(
    token: u64,
    op: u64,
    record: EpollInterestRecord,
) -> Result<u64, EpollError> {
    EPOLL_REGISTRY.lock().ctl(token, op, record)
}

pub fn epoll_snapshot(
    token: u64,
    maxevents: usize,
) -> Result<(u64, Vec<(EpollInterestRecord, EpollReportState)>), EpollError> {
    EPOLL_REGISTRY.lock().snapshot(token, maxevents)
}

pub fn epoll_claim(
    token: u64,
    key: EpollInterestKey,
    generation: u64,
) -> Result<bool, EpollError> {
    EPOLL_REGISTRY.lock().claim(token, key, generation)
}

pub fn epoll_retire(token: u64) -> Option<u64> {
    EPOLL_REGISTRY.lock().retire(token)
}

pub fn epoll_purge_object(provider: u16, object_id: u64) -> Vec<(u64, u64)> {
    EPOLL_REGISTRY.lock().purge_object(provider, object_id)
}

#[derive(Debug)]
struct EpollOpenDescription {
    token: u64,
    descriptor_refs: AtomicUsize,
}

#[derive(Debug, Clone)]
pub struct EpollHandle {
    description: Arc<EpollOpenDescription>,
}

impl EpollHandle {
    pub fn new() -> Self {
        let mut bytes = [0_u8; 8];
        nucleus_core::util::random::Random::new().fill_bytes(&mut bytes);
        Self {
            description: Arc::new(EpollOpenDescription {
                token: u64::from_le_bytes(bytes).max(1),
                descriptor_refs: AtomicUsize::new(1),
            }),
        }
    }

    /// Allocates a unique kernel-provider token and registers a new epoll
    /// instance in `EPOLL_REGISTRY`.
    pub fn create(charge_group: Option<usize>) -> Result<Self, EpollError> {
        let token = crate::user::local_wait::allocate_token().ok_or(EpollError::Capacity)?;
        epoll_create(token, charge_group)?;
        Ok(Self::from_token(token))
    }

    pub fn from_token(token: u64) -> Self {
        assert!(token != 0, "epoll open-description token must be nonzero");
        Self {
            description: Arc::new(EpollOpenDescription {
                token,
                descriptor_refs: AtomicUsize::new(1),
            }),
        }
    }

    pub fn token_id(&self) -> u64 {
        self.description.token
    }

    /// Pins one live fd-table reference.
    ///
    /// `Arc` clones are transient kernel snapshots and deliberately do not
    /// affect this count. Only a descriptor-table publication may call this.
    pub fn try_acquire_descriptor_reference(&self) -> bool {
        // ORDERING: AcqRel linearizes descriptor publication against final
        // release; Acquire on failure observes the zero terminal state.
        self.description
            .descriptor_refs
            // ORDERING: see the retain linearization contract above.
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current != 0).then(|| current.checked_add(1)).flatten()
            })
            .is_ok()
    }

    /// Drops one published fd-table reference and reports final retirement.
    ///
    /// A zero-to-live transition would resurrect a retired epoll instance,
    /// so underflow and resurrection are hard invariants.
    pub fn release_descriptor_reference(&self) -> bool {
        // ORDERING: AcqRel makes the final zero transition the unique retire
        // authority after all earlier descriptor publications.
        let previous = self
            .description
            .descriptor_refs
            // ORDERING: see the final-retire linearization contract above.
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_sub(1)
            })
            .expect("epoll descriptor reference count underflow");
        previous == 1
    }

    pub fn is_last_reference(&self) -> bool {
        // ORDERING: Acquire observes the final AcqRel release before cleanup
        // derives registry retirement from this snapshot.
        self.description.descriptor_refs.load(Ordering::Acquire) == 0
    }
}

impl PartialEq for EpollHandle {
    fn eq(&self, other: &Self) -> bool {
        self.token_id() == other.token_id()
    }
}

impl Eq for EpollHandle {}

impl Default for EpollHandle {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        EpollError, EpollHandle, EpollInterestKey, EpollInterestRecord, EpollRegistry,
        EpollReportState, epoll_ctl, epoll_retire, epoll_snapshot,
    };
    use rustos_user_abi::linux as linux_abi;
    use rustos_user_abi::performance::WAITSET_MAX_EPOLL_OBJECTS;
    use rustos_user_abi::syscall::WAITSET_MAX_INTERESTS;

    fn record(
        target_fd: u64,
        provider: u16,
        object_id: u64,
        provider_epoch: u64,
        events: u32,
        data: u64,
    ) -> EpollInterestRecord {
        EpollInterestRecord {
            key: EpollInterestKey {
                target_fd,
                provider,
                object_id,
            },
            provider_epoch,
            events,
            data,
        }
    }

    #[test]
    fn descriptor_references_are_explicit_and_transient_clones_do_not_count() {
        let epoll = EpollHandle::new();
        let snapshot = epoll.clone();
        assert!(epoll.try_acquire_descriptor_reference());
        assert!(!snapshot.release_descriptor_reference());
        assert!(epoll.release_descriptor_reference());
        assert!(epoll.is_last_reference());
        assert!(!snapshot.try_acquire_descriptor_reference());
    }

    #[test]
    fn epoll_object_cap_boundary_rejects_overflow_and_recovers_after_retire() {
        let mut reg = EpollRegistry::new();
        for i in 1..=(WAITSET_MAX_EPOLL_OBJECTS as u64) {
            assert_eq!(reg.create(i, None), Ok(1));
        }
        assert_eq!(
            reg.create(WAITSET_MAX_EPOLL_OBJECTS as u64 + 1, None),
            Err(EpollError::Capacity)
        );
        assert_eq!(reg.create(0, None), Err(EpollError::Exists));
        assert_eq!(reg.create(1, None), Err(EpollError::Exists));

        assert_eq!(reg.retire(1), Some(2));
        assert_eq!(reg.retire(1), None);
        assert_eq!(reg.create(WAITSET_MAX_EPOLL_OBJECTS as u64 + 1, None), Ok(1));
    }

    #[test]
    fn epoll_interest_cap_boundary_rejects_overflow_and_recovers_after_del() {
        let mut reg = EpollRegistry::new();
        assert_eq!(reg.create(10, None), Ok(1));
        for i in 0..(WAITSET_MAX_INTERESTS as u64) {
            let rec = record(i, 1, 100 + i, 1, linux_abi::EPOLLIN, i);
            assert!(reg.ctl(10, linux_abi::EPOLL_CTL_ADD, rec).is_ok());
        }
        let extra = record(
            WAITSET_MAX_INTERESTS as u64,
            1,
            100 + WAITSET_MAX_INTERESTS as u64,
            1,
            linux_abi::EPOLLIN,
            999,
        );
        assert_eq!(
            reg.ctl(10, linux_abi::EPOLL_CTL_ADD, extra),
            Err(EpollError::Capacity)
        );
        // Deleting one frees a slot.
        let first = record(0, 1, 100, 1, linux_abi::EPOLLIN, 0);
        assert!(reg.ctl(10, linux_abi::EPOLL_CTL_DEL, first).is_ok());
        assert!(reg.ctl(10, linux_abi::EPOLL_CTL_ADD, extra).is_ok());
    }

    #[test]
    fn snapshot_rotates_round_robin_cursor_across_calls() {
        let mut reg = EpollRegistry::new();
        assert_eq!(reg.create(20, None), Ok(1));
        for fd in [1_u64, 2, 3] {
            let rec = record(fd, 2, 10 + fd, 1, linux_abi::EPOLLIN, fd * 10);
            assert!(reg.ctl(20, linux_abi::EPOLL_CTL_ADD, rec).is_ok());
        }
        let (_, s1) = reg.snapshot(20, 3).unwrap();
        let (_, s2) = reg.snapshot(20, 3).unwrap();
        let (_, s3) = reg.snapshot(20, 3).unwrap();
        let (_, s4) = reg.snapshot(20, 3).unwrap();
        let fds = |s: &[(EpollInterestRecord, EpollReportState)]| -> alloc::vec::Vec<u64> {
            s.iter().map(|(r, _)| r.key.target_fd).collect()
        };
        assert_eq!(fds(&s1), alloc::vec![1, 2, 3]);
        assert_eq!(fds(&s2), alloc::vec![2, 3, 1]);
        assert_eq!(fds(&s3), alloc::vec![3, 1, 2]);
        assert_eq!(fds(&s4), alloc::vec![1, 2, 3]);
    }

    #[test]
    fn provider_restart_epoch_update_replaces_without_duplicating_identity() {
        let mut reg = EpollRegistry::new();
        assert_eq!(reg.create(30, None), Ok(1));
        let initial = record(5, 2, 77, 1, linux_abi::EPOLLIN | linux_abi::EPOLLET, 111);
        assert_eq!(reg.ctl(30, linux_abi::EPOLL_CTL_ADD, initial), Ok(2));

        // Duplicate ADD with a newer provider epoch must still fail with Exists.
        let dup_new_epoch = record(5, 2, 77, 2, linux_abi::EPOLLIN, 222);
        assert_eq!(
            reg.ctl(30, linux_abi::EPOLL_CTL_ADD, dup_new_epoch),
            Err(EpollError::Exists)
        );

        // Claim an edge at generation 4, then MOD with new epoch 2: must update
        // epoch in place and clear edge report state without duplicating.
        assert_eq!(reg.claim(30, initial.key, 4), Ok(true));
        assert_eq!(reg.ctl(30, linux_abi::EPOLL_CTL_MOD, dup_new_epoch), Ok(3));
        let (gen_after, entries) = reg.snapshot(30, 8).unwrap();
        assert_eq!(gen_after, 3);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0.provider_epoch, 2);
        assert_eq!(entries[0].0.data, 222);
        assert_eq!(entries[0].1, EpollReportState::default());
    }

    #[test]
    fn reverse_index_purge_removes_exact_object_and_bumps_affected_generations() {
        let mut reg = EpollRegistry::new();
        assert_eq!(reg.create(40, None), Ok(1));
        assert_eq!(reg.create(41, None), Ok(1));
        assert_eq!(reg.create(42, None), Ok(1));

        // Epoll 40 watches (provider=2, obj=99) on two duped fds plus obj=100.
        let r1 = record(3, 2, 99, 1, linux_abi::EPOLLIN, 1);
        let r2 = record(4, 2, 99, 1, linux_abi::EPOLLOUT, 2);
        let r3 = record(5, 2, 100, 1, linux_abi::EPOLLIN, 3);
        // Epoll 41 watches (provider=2, obj=99).
        let r4 = record(6, 2, 99, 1, linux_abi::EPOLLIN, 4);
        // Epoll 42 watches (provider=3, obj=99) — different provider, must survive!
        let r5 = record(7, 3, 99, 1, linux_abi::EPOLLIN, 5);

        assert!(reg.ctl(40, linux_abi::EPOLL_CTL_ADD, r1).is_ok());
        assert!(reg.ctl(40, linux_abi::EPOLL_CTL_ADD, r2).is_ok());
        assert!(reg.ctl(40, linux_abi::EPOLL_CTL_ADD, r3).is_ok());
        assert!(reg.ctl(41, linux_abi::EPOLL_CTL_ADD, r4).is_ok());
        assert!(reg.ctl(42, linux_abi::EPOLL_CTL_ADD, r5).is_ok());

        let wakes = reg.purge_object(2, 99);
        // Epoll 40 had gen 4 before purge -> 2 removed -> gen 6 (collapsed to 1 wake).
        // Epoll 41 had gen 2 before purge -> 1 removed -> gen 3.
        // Epoll 42 unaffected.
        assert_eq!(wakes, alloc::vec![(40, 6), (41, 3)]);
        assert_eq!(reg.purge_object(2, 99), alloc::vec![]);

        let (_, e40) = reg.snapshot(40, 8).unwrap();
        assert_eq!(e40.len(), 1);
        assert_eq!(e40[0].0.key, r3.key);

        let (_, e41) = reg.snapshot(41, 8).unwrap();
        assert!(e41.is_empty());

        let (g42, e42) = reg.snapshot(42, 8).unwrap();
        assert_eq!(g42, 2);
        assert_eq!(e42.len(), 1);
        assert_eq!(e42[0].0.key, r5.key);
    }

    #[test]
    fn claim_enforces_epollet_and_epolloneshot_and_mod_rearms() {
        let mut reg = EpollRegistry::new();
        assert_eq!(reg.create(50, None), Ok(1));

        let et = record(1, 2, 10, 1, linux_abi::EPOLLIN | linux_abi::EPOLLET, 1);
        let oneshot = record(2, 2, 11, 1, linux_abi::EPOLLIN | linux_abi::EPOLLONESHOT, 2);
        let level = record(3, 2, 12, 1, linux_abi::EPOLLIN, 3);
        let exclusive = record(
            4,
            2,
            13,
            1,
            linux_abi::EPOLLIN | linux_abi::EPOLLEXCLUSIVE,
            4,
        );

        assert!(reg.ctl(50, linux_abi::EPOLL_CTL_ADD, et).is_ok());
        assert!(reg.ctl(50, linux_abi::EPOLL_CTL_ADD, oneshot).is_ok());
        assert!(reg.ctl(50, linux_abi::EPOLL_CTL_ADD, level).is_ok());
        assert!(reg.ctl(50, linux_abi::EPOLL_CTL_ADD, exclusive).is_ok());

        // EPOLLEXCLUSIVE cannot be modified.
        assert_eq!(
            reg.ctl(50, linux_abi::EPOLL_CTL_MOD, exclusive),
            Err(EpollError::InvalidArgument)
        );

        // Edge-triggered: generation 5 wins once; stale or equal gen fails; newer gen wins.
        assert_eq!(reg.claim(50, et.key, 5), Ok(true));
        assert_eq!(reg.claim(50, et.key, 5), Ok(false));
        assert_eq!(reg.claim(50, et.key, 4), Ok(false));
        assert_eq!(reg.claim(50, et.key, 6), Ok(true));

        // One-shot: wins once then stays disabled even for newer generations until MOD.
        assert_eq!(reg.claim(50, oneshot.key, 1), Ok(true));
        assert_eq!(reg.claim(50, oneshot.key, 2), Ok(false));
        assert!(reg.ctl(50, linux_abi::EPOLL_CTL_MOD, oneshot).is_ok());
        assert_eq!(reg.claim(50, oneshot.key, 1), Ok(true));

        // Level-triggered: always returns true.
        assert_eq!(reg.claim(50, level.key, 1), Ok(true));
        assert_eq!(reg.claim(50, level.key, 1), Ok(true));
    }

    #[test]
    fn retire_cleans_up_interests_and_reverse_index() {
        let mut reg = EpollRegistry::new();
        assert_eq!(reg.create(60, None), Ok(1));
        let rec = record(1, 2, 555, 1, linux_abi::EPOLLIN, 1);
        assert_eq!(reg.ctl(60, linux_abi::EPOLL_CTL_ADD, rec), Ok(2));

        assert_eq!(reg.retire(60), Some(3));
        assert_eq!(reg.snapshot(60, 8), Err(EpollError::NotFound));
        assert_eq!(reg.claim(60, rec.key, 1), Err(EpollError::NotFound));
        // Reverse index bucket was cleaned up, so purging yields no wakes and
        // never panics on a dangling epoll token.
        assert_eq!(reg.purge_object(2, 555), alloc::vec![]);
    }

    #[test]
    fn handle_create_registers_in_global_registry_and_retires_cleanly() {
        let handle = EpollHandle::create(None).expect("create epoll handle");
        let token = handle.token_id();
        let rec = record(9, 5, 9999, 1, linux_abi::EPOLLIN, 42);
        assert_eq!(epoll_ctl(token, linux_abi::EPOLL_CTL_ADD, rec), Ok(2));
        let (generation, entries) = epoll_snapshot(token, 8).expect("snapshot");
        assert_eq!(generation, 2);
        assert_eq!(entries.len(), 1);
        assert_eq!(epoll_retire(token), Some(3));
        assert_eq!(epoll_snapshot(token, 8), Err(EpollError::NotFound));
    }

    #[test]
    fn epoll_instance_and_interest_pages_are_charged_and_refunded_exactly_once() {
        use kernel_mm::api::charge;
        // Charge group 56 is reserved for this test.
        let g = 56;
        charge::init_group(g, None);
        charge::set_limit(g, 3);
        let mut reg = EpollRegistry::new();
        assert_eq!(reg.create(1, Some(g)), Ok(1));
        assert_eq!(charge::usage(g), 1);
        assert_eq!(reg.create(2, Some(g)), Ok(1));
        assert_eq!(charge::usage(g), 2);
        // 16 interests fit the object's first interest page: 1 + 1 = 2 pages.
        for fd in 0..16 {
            reg.ctl(1, linux_abi::EPOLL_CTL_ADD, record(fd, 1, fd + 1, 1, linux_abi::EPOLLIN, 0))
                .unwrap();
        }
        assert_eq!(charge::usage(g), 3);
        // The 17th needs a second interest page: refused, nothing changes.
        assert_eq!(
            reg.ctl(1, linux_abi::EPOLL_CTL_ADD, record(16, 1, 17, 1, linux_abi::EPOLLIN, 0)),
            Err(EpollError::Capacity)
        );
        assert_eq!(charge::usage(g), 3);
        assert_eq!(reg.create(3, Some(g)), Err(EpollError::Capacity));
        assert_eq!(charge::usage(g), 3);
        // DEL shrinks; purge shrinks; retire refunds the rest exactly once.
        reg.ctl(1, linux_abi::EPOLL_CTL_DEL, record(0, 1, 1, 0, 0, 0)).unwrap();
        assert_eq!(charge::usage(g), 3);
        reg.purge_object(1, 2);
        reg.purge_object(1, 3);
        reg.purge_object(1, 4);
        reg.purge_object(1, 5);
        reg.purge_object(1, 6);
        reg.purge_object(1, 7);
        reg.purge_object(1, 8);
        reg.purge_object(1, 9);
        reg.purge_object(1, 10);
        reg.purge_object(1, 11);
        reg.purge_object(1, 12);
        reg.purge_object(1, 13);
        reg.purge_object(1, 14);
        reg.purge_object(1, 15);
        reg.purge_object(1, 16);
        assert_eq!(charge::usage(g), 2, "empty interest set keeps only the object page");
        assert!(reg.retire(1).is_some());
        assert_eq!(charge::usage(g), 1);
        assert!(reg.retire(1).is_none());
        assert_eq!(charge::usage(g), 1, "double retire refunds nothing");
        assert!(reg.retire(2).is_some());
        assert_eq!(charge::usage(g), 0);
        assert_eq!(charge::retainers(g), 0);
    }
}
