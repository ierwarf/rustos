//! Qualitative ceilings. Writers hold PROCESS_TABLE; readers test one cached
//! word and exact job incarnation without allocation or a table/registry lock.
//! A ceiling is not a positive broker grant.
//!
//! - Owner: PROCESS_TABLE serializes own masks and every descendant refresh.
//! - Boundary: typed SET_POLICY authority is checked at the syscall edge;
//!   raw JobIds and permitted ceiling bits grant no positive subject authority.
//! - Lifecycle: creation publishes ALL intersected with live ancestors; kill
//!   invalidates caches before completion, reuse changes the exact generation.
//! - Concurrency: one operation word is observed old or new during updates;
//!   whole masks require the table guard, not a 256-bit atomic snapshot.
//! - Failure: missing, stale, killed and out-of-range checks refuse.
//! - Forbidden: no allocation or registry/table lock on the cached read path.
//! - Evidence: job-policy-lifecycle and job-policy-publication models, native
//!   hierarchy/reuse tests and bounded Loom/Shuttle/herd projections.
use core::sync::atomic::{AtomicU64, Ordering};
use super::{JobError, JobId, JobPolicy, JobState, JOB_MAX_DEPTH, JOB_SLOTS, NO_PARENT, ROOT_JOB};
use super::super::{ProcessTable, PROCESS_TABLE};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JobPolicyInfo {
    pub own: JobPolicy,
    pub effective: JobPolicy,
}

struct PublishedPolicy {
    incarnation: AtomicU64,
    words: [AtomicU64; 4],
}

impl PublishedPolicy {
    const fn empty() -> Self {
        Self { incarnation: AtomicU64::new(0), words: [const { AtomicU64::new(0) }; 4] }
    }

    const fn root() -> Self {
        Self {
            incarnation: AtomicU64::new(ROOT_JOB.to_raw()),
            words: [const { AtomicU64::new(u64::MAX) }; 4],
        }
    }

    fn invalidate(&self) {
        // ORDERING: Release revokes this incarnation before its slot can be
        // recycled or a completed kill can be observed by subsequent checks.
        self.incarnation.store(0, Ordering::Release);
    }

    /// Only PROCESS_TABLE's writer may publish or replace this cache slot.
    fn publish(&self, job: JobId, policy: JobPolicy) {
        if self.incarnation.load(Ordering::Relaxed) != job.to_raw() {
            self.invalidate();
        }
        for (word, value) in self.words.iter().zip(policy.words) {
            // ORDERING: A reader seeing a replacement payload acquires the
            // preceding incarnation revocation before its final identity check.
            // For the same incarnation, each bit decision sees old or new data.
            word.store(value, Ordering::Release);
        }
        // ORDERING: Release commits the new incarnation after all four payload
        // words. Same-incarnation updates deliberately do not revoke validity.
        self.incarnation.store(job.to_raw(), Ordering::Release);
    }

    fn allows(&self, job: JobId, operation: u16) -> bool {
        self.allows_with_hook(job, operation, || {})
    }

    // Production supplies only the no-op above. Tests inject the exact
    // replacement window without relying on a probabilistic host race.
    fn allows_with_hook(&self, job: JobId, operation: u16, after_payload: impl FnOnce()) -> bool {
        let operation = usize::from(operation);
        if operation >= 256 || job.to_raw() == 0 {
            return false;
        }
        // ORDERING: Acquire observes the complete publication of this exact
        // job incarnation; zero and a different generation never authorize.
        let before = self.incarnation.load(Ordering::Acquire);
        if before != job.to_raw() {
            return false;
        }
        // ORDERING: Acquire joins a replacement payload's preceding revocation
        // to the final identity read; a new tenant cannot authorize an old id.
        let word = self.words[operation / 64].load(Ordering::Acquire);
        after_payload();
        // ORDERING: Acquire revalidates incarnation after the payload read.
        // Job generation exhaustion retires a slot, so identity cannot wrap.
        let after = self.incarnation.load(Ordering::Acquire);
        before == after && word & (1_u64 << (operation % 64)) != 0
    }

    /// Whole-mask coherence comes from the caller's PROCESS_TABLE guard, not
    /// from treating four independent atomic words as a 256-bit atomic value.
    fn snapshot_locked(&self, job: JobId, _table: &ProcessTable) -> Option<JobPolicy> {
        // ORDERING: Acquire resolves the exact writer-published incarnation.
        if self.incarnation.load(Ordering::Acquire) != job.to_raw() {
            return None;
        }
        Some(JobPolicy { words: core::array::from_fn(|index| {
            // ORDERING: Acquire observes this word's latest serialized update;
            // PROCESS_TABLE excludes all concurrent cache writers here.
            self.words[index].load(Ordering::Acquire)
        }) })
    }
}

const fn initial_cache() -> [PublishedPolicy; JOB_SLOTS] {
    let mut cache = [const { PublishedPolicy::empty() }; JOB_SLOTS];
    cache[0] = PublishedPolicy::root();
    cache
}

static EFFECTIVE_POLICY: [PublishedPolicy; JOB_SLOTS] = initial_cache();

fn effective_locked(table: &ProcessTable, job: JobId) -> JobPolicy {
    let mut index = job.index as usize;
    let mut effective = JobPolicy::ALL;
    for _ in 0..=JOB_MAX_DEPTH {
        let Some(slot) = table.jobs.get(index) else { return JobPolicy::EMPTY };
        if !slot.in_use || slot.state != JobState::Live {
            return JobPolicy::EMPTY;
        }
        effective = effective.intersection(slot.policy);
        if slot.parent == NO_PARENT {
            return if index == 0 { effective } else { JobPolicy::EMPTY };
        }
        index = slot.parent as usize;
    }
    JobPolicy::EMPTY
}

pub(super) fn publish_job(table: &ProcessTable, job: JobId) {
    if table.jobs[job.index as usize].state == JobState::Killing {
        invalidate_job(job.index as usize);
    } else {
        EFFECTIVE_POLICY[job.index as usize].publish(job, effective_locked(table, job));
    }
}

pub(super) fn invalidate_job(index: usize) {
    EFFECTIVE_POLICY[index].invalidate();
}

fn info_locked(table: &ProcessTable, job: JobId) -> Result<JobPolicyInfo, JobError> {
    let slot = table.job_slot(job).ok_or(JobError::Stale)?;
    let effective = if slot.state == JobState::Killing {
        JobPolicy::EMPTY
    } else {
        EFFECTIVE_POLICY[job.index as usize].snapshot_locked(job, table).ok_or(JobError::Stale)?
    };
    Ok(JobPolicyInfo { own: slot.policy, effective })
}

pub fn job_policy_info(job: JobId) -> Result<JobPolicyInfo, JobError> {
    info_locked(&PROCESS_TABLE.lock(), job)
}

/// Own masks may be wider than their ancestors. Every existing descendant is
/// refreshed before return; each racing single-bit check may see its old or new
/// word. Neither the four words nor the whole subtree are one atomic snapshot.
pub fn job_set_policy(job: JobId, own: JobPolicy) -> Result<JobPolicyInfo, JobError> {
    let mut table = PROCESS_TABLE.lock();
    let slot = table.job_slot(job).ok_or(JobError::Stale)?;
    if slot.state == JobState::Killing {
        return Err(JobError::Killing);
    }
    table.jobs[job.index as usize].policy = own;
    for index in 0..JOB_SLOTS {
        if table.jobs[index].in_use && table.job_contains(job.index, index as u32) {
            publish_job(&table, table.job_id_of(index as u32));
        }
    }
    info_locked(&table, job)
}

/// Bounded lock-free ceiling check. A raw JobId never grants authority; callers
/// must independently possess the exact process/broker authority they enforce.
pub fn job_policy_allows(job: JobId, operation: u16) -> bool {
    EFFECTIVE_POLICY.get(job.index as usize)
        .is_some_and(|cache| cache.allows(job, operation))
}

#[cfg(test)]
pub(super) fn reset_for_tests() {
    for cache in &EFFECTIVE_POLICY { cache.invalidate(); }
    EFFECTIVE_POLICY[0].publish(ROOT_JOB, JobPolicy::ALL);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::multitask::process_table::{self, jobs::*, spawn, ProcessHandle};
    use crate::multitask::process_table::tests::{isolate_process_table, new_state};

    fn spawn_in(pid: u64, parent: Option<u64>, job: JobId) -> ProcessHandle {
        let reservation = process_table::reserve_spawn_in(Some(job)).unwrap();
        spawn::publish_spawn(reservation, pid, parent, new_state()).unwrap()
    }

    #[test]
    fn live_parent_policy_refreshes_existing_descendants_and_new_children_in_all_words() {
        let _isolation = isolate_process_table();
        assert_eq!(job_policy_info(ROOT_JOB).unwrap().effective, JobPolicy::ALL);
        let parent = job_create_child(ROOT_JOB).unwrap();
        let child = job_create_child(parent).unwrap();
        let sibling = job_create_child(ROOT_JOB).unwrap();
        let mask = JobPolicy { words: [0xff, 0xff00, 0xff0000, 0xff000000] };
        assert_eq!(job_set_policy(parent, mask).unwrap().effective, mask);
        assert_eq!(job_policy_info(child).unwrap().own, JobPolicy::ALL);
        assert_eq!(job_policy_info(child).unwrap().effective, mask);
        assert_eq!(job_policy_info(sibling).unwrap().effective, JobPolicy::ALL);
        for operation in 0..256 {
            assert_eq!(job_policy_allows(child, operation), mask.allows(operation));
            assert!(job_policy_allows(sibling, operation));
        }
        let child_mask = JobPolicy { words: [0x33, 0x3300, 0x330000, 0x33000000] };
        let root_mask = JobPolicy { words: [0x0f, 0x0f00, 0x0f0000, 0x0f000000] };
        job_set_policy(child, child_mask).unwrap();
        job_set_policy(ROOT_JOB, root_mask).unwrap();
        let effective = mask.intersection(child_mask).intersection(root_mask);
        assert_eq!(job_policy_info(child).unwrap().effective, effective);
        for operation in 0..256 {
            assert_eq!(job_policy_allows(child, operation), effective.allows(operation));
        }
        let late = job_create_child(parent).unwrap();
        assert_eq!(job_policy_info(late).unwrap().effective, mask.intersection(root_mask));
        assert_eq!(job_policy_info(sibling).unwrap().effective, root_mask);
    }

    #[test]
    fn kill_denies_every_cached_descendant_and_root_updates_cannot_resurrect_it() {
        let _isolation = isolate_process_table();
        let parent = job_create_child(ROOT_JOB).unwrap();
        let child = job_create_child(parent).unwrap();
        let sibling = job_create_child(ROOT_JOB).unwrap();
        let _member = spawn_in(401, None, child);
        assert!(job_policy_allows(parent, 255));
        assert!(job_policy_allows(child, 255));
        job_kill_begin(parent).unwrap();
        assert!(!job_policy_allows(parent, 255));
        assert!(!job_policy_allows(child, 255));
        assert_eq!(job_policy_info(child).unwrap().effective, JobPolicy::EMPTY);
        assert_eq!(job_set_policy(child, JobPolicy::ALL), Err(JobError::Killing));
        job_set_policy(ROOT_JOB, JobPolicy::ALL).unwrap();
        assert!(!job_policy_allows(parent, 0));
        assert!(!job_policy_allows(child, 0));
        assert!(job_policy_allows(sibling, 0));
    }

    #[test]
    fn recycled_and_exhausted_job_generations_never_reuse_cached_authority() {
        let _isolation = isolate_process_table();
        let old = job_create_child(ROOT_JOB).unwrap();
        job_set_policy(old, JobPolicy::EMPTY).unwrap();
        job_kill_begin(old).unwrap();
        let new = job_create_child(ROOT_JOB).unwrap();
        assert_eq!(old.index(), new.index());
        assert_ne!(old, new);
        assert_eq!(job_policy_info(new).unwrap().own, JobPolicy::ALL);
        assert!(job_policy_allows(new, 255));
        assert!(!job_policy_allows(old, 255));
        assert_eq!(job_set_policy(old, JobPolicy::ALL), Err(JobError::Stale));
        let exhausted = {
            let mut table = PROCESS_TABLE.lock();
            table.jobs[new.index as usize].generation = u32::MAX;
            let exhausted = table.job_id_of(new.index);
            publish_job(&table, exhausted);
            exhausted
        };
        assert!(!job_policy_allows(new, 255));
        assert!(job_policy_allows(exhausted, 255));
        job_kill_begin(exhausted).unwrap();
        let other = job_create_child(ROOT_JOB).unwrap();
        assert_ne!(other.index(), exhausted.index());
        assert!(!job_policy_allows(exhausted, 255));
    }

    #[test]
    fn published_membership_is_exact_and_refuses_hidden_exec_or_stale_process_publication() {
        let _isolation = isolate_process_table();
        let job = job_create_child(ROOT_JOB).unwrap();
        let handle = spawn_in(402, None, job);
        let fork = {
            let reservation = process_table::reserve_spawn_in(None).unwrap();
            spawn::publish_spawn(reservation, 403, Some(402), new_state()).unwrap()
        };
        assert_eq!(process_table::published_process_job(handle), Some(job));
        assert_eq!(process_table::published_process_job(fork), Some(job));
        let stale = ProcessHandle::new(handle.index(), handle.generation() + 1);
        assert_eq!(process_table::published_process_job(stale), None);
        let mut table = PROCESS_TABLE.lock();
        table.slots[handle.index()].object.as_mut().unwrap().exec_in_progress = true;
        process_table::publish_slot_visibility(handle.index(), &table.slots[handle.index()]);
        assert_eq!(process_table::published_process_job(handle), None);
        let object = table.slots[handle.index()].object.as_mut().unwrap();
        object.exec_in_progress = false;
        object.mm_generation += 1;
        process_table::publish_slot_visibility(handle.index(), &table.slots[handle.index()]);
        assert_eq!(process_table::published_process_job(handle), Some(job));
        table.slots[handle.index()].object.as_mut().unwrap().exiting = true;
        process_table::publish_slot_visibility(handle.index(), &table.slots[handle.index()]);
        assert_eq!(process_table::published_process_job(handle), None);
    }

    #[test]
    fn current_binding_ceiling_read_never_borrows_table_or_recovers_hidden_membership() {
        let _isolation = isolate_process_table();
        let job = job_create_child(ROOT_JOB).unwrap();
        job_set_policy(job, JobPolicy::ALL.with_operation(255, false).unwrap()).unwrap();
        let handle = spawn_in(404, None, job);
        let _table = PROCESS_TABLE.lock();
        assert!(crate::multitask::current::policy_for_binding(handle, 0));
        assert!(!crate::multitask::current::policy_for_binding(handle, 255));
        assert!(!crate::multitask::current::policy_for_binding(handle, 256));
        process_table::clear_publication_for_test(handle.index());
        assert!(!crate::multitask::current::policy_for_binding(handle, 0));
    }

    #[test]
    fn policy_cache_checks_all_words_and_rejects_missing_stale_or_revoked_identity() {
        let cache = PublishedPolicy::empty();
        let old = JobId::from_raw((7_u64 << 32) | 1);
        let new = JobId::from_raw((7_u64 << 32) | 2);
        assert!(!cache.allows(old, 0));
        let policy = JobPolicy { words: [1, 2, 4, 1_u64 << 63] };
        cache.publish(old, policy);
        for operation in [0, 65, 130, 255] { assert!(cache.allows(old, operation)); }
        for operation in [1, 64, 129, 254, 256, u16::MAX] { assert!(!cache.allows(old, operation)); }
        cache.invalidate();
        assert!(!cache.allows(old, 0));
        cache.publish(new, JobPolicy::ALL);
        assert!(!cache.allows(old, 0));
        assert!(cache.allows(new, 255));
        cache.publish(new, JobPolicy::EMPTY);
        assert!(!cache.allows(new, 255));
        assert!(!cache.allows(JobId::from_raw(0), 0));
        assert!(!job_policy_allows(JobId::from_raw(u64::MAX), 0));
        cache.publish(old, JobPolicy::ALL);
        assert!(!cache.allows_with_hook(old, 0, || cache.publish(new, JobPolicy::EMPTY)));
    }
}
