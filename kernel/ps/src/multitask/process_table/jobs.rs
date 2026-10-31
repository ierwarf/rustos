//! Job tree: group lifetime and attribution for processes.
//!
//! - **Owner:** `kernel-ps`, inside the process-table lock. A job slot and the
//!   membership of the processes in it change under the same lock, so no
//!   process can join a job that is being killed.
//! - **Boundary:** a `JobId` is `(slot, generation)`; a recycled slot never
//!   aliases a stale id. The kernel knows jobs, never components or names.
//! - **Lifecycle:** `Live -> Killing -> (slot recycled)`. The root job is created
//!   at boot, is never killed, and is never recycled.
//! - **Concurrency:** one process belongs to one job for life; exec and fork keep
//!   it. Membership counts are derived by scanning the process slots, so there is
//!   no counter that can drift from the processes themselves.
//! - **Failure:** a stale id, a `Killing` job, a full slab, and excess depth all
//!   refuse; none partially applies.
//! - **Forbidden:** moving a process between jobs, killing the root job, or
//!   publishing a process into a `Killing` job.
//! - **Evidence:** `job-lifecycle`.
use super::*;
mod policy;
pub use rustos_user_abi::syscall::RustosJobPolicy as JobPolicy;
pub use policy::{JobPolicyInfo, job_policy_allows, job_policy_info, job_set_policy};

/// Slab capacity of the job table. An internal bound of this implementation, not
/// an ABI value: it moves into the root job's `jobs.max` policy when the process
/// table gets its directory (see `docs/architecture/job-model.md`, phase 2a).
pub(super) const JOB_SLOTS: usize = 64;
// One charge group per job slot, indexed the same way.
const _: () = assert!(JOB_SLOTS == kernel_mm::api::charge::CHARGE_GROUPS);
/// Deepest nesting below the root job.
pub const JOB_MAX_DEPTH: u8 = 8;

const NO_PARENT: u32 = u32::MAX;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct JobId {
    index: u32,
    generation: u32,
}

/// The job every process belongs to until a supervisor places it elsewhere.
pub const ROOT_JOB: JobId = JobId {
    index: 0,
    generation: 1,
};

impl JobId {
    pub(super) const fn index(self) -> u32 {
        self.index
    }

    pub const fn from_raw(raw: u64) -> Self {
        Self {
            index: (raw >> 32) as u32,
            generation: raw as u32,
        }
    }

    pub const fn to_raw(self) -> u64 {
        ((self.index as u64) << 32) | self.generation as u64
    }

    /// Descriptor identity adapter only; the job table still resolves liveness.
    pub(crate) const fn handle_token(self) -> Option<kernel_object::api::handle::HandleToken> {
        kernel_object::api::handle::HandleToken::from_job(self.index, self.generation)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JobState {
    Live,
    Killing,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JobError {
    /// The id names no live job: never existed, or recycled since.
    Stale,
    /// The job is being killed and admits no new member or child.
    Killing,
    TooDeep,
    Full,
    /// The operation would remove the root job.
    Root,
}

/// A dimension no limit applies to; the job inherits its parent's.
pub const LIMIT_UNSET: u64 = u64::MAX;

/// Counted limits of one job, `LIMIT_UNSET` meaning inherit. Memory is absent on
/// purpose: charging it needs a hook in the frame-descriptor ledger, and a limit
/// that is stored but not enforced would be worse than none.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JobLimits {
    /// Process objects not yet reaped, in this job and every descendant.
    pub processes: u64,
    /// Threads of those processes.
    pub threads: u64,
    /// Resident user data pages (4 KiB) mapped by the job's processes and its
    /// descendants, charged where the frame-descriptor ledger counts them.
    /// Page-table frames and kernel objects are not charged.
    pub memory_pages: u64,
}

impl JobLimits {
    pub const UNSET: Self =
        Self { processes: LIMIT_UNSET, threads: LIMIT_UNSET, memory_pages: LIMIT_UNSET };
}

/// Which dimensions a `job_set_limits` call changes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LimitMask(u32);

impl LimitMask {
    pub const PROCESSES: Self = Self(1 << 0);
    pub const THREADS: Self = Self(1 << 1);
    pub const MEMORY: Self = Self(1 << 2);

    pub const fn from_bits(bits: u32) -> Option<Self> {
        if bits == 0 || bits & !(Self::PROCESSES.0 | Self::THREADS.0 | Self::MEMORY.0) != 0 {
            None
        } else {
            Some(Self(bits))
        }
    }

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JobInfo {
    pub id: JobId,
    pub parent: Option<JobId>,
    pub depth: u8,
    pub state: JobState,
    /// Live processes directly in this job.
    pub processes: u64,
    /// Live processes in this job and every descendant.
    pub subtree_processes: u64,
    /// Live jobs directly below this one.
    pub children: u64,
    pub kills: u64,
    /// As configured on this job.
    pub limits: JobLimits,
    /// `min` of this job's and every ancestor's own limit.
    pub effective: JobLimits,
    /// Unreaped process objects and their threads in the subtree.
    pub usage_processes: u64,
    pub usage_threads: u64,
    /// Pages currently charged to the subtree.
    pub usage_memory_pages: u64,
}

#[derive(Clone, Copy)]
pub(super) struct JobSlot {
    generation: u32,
    in_use: bool,
    parent: u32,
    depth: u8,
    state: JobState,
    kills: u64,
    limits: JobLimits,
    policy: JobPolicy,
    /// Direct + descendants; changed only under the process-table lock.
    /// The Ring3 supervisor retains exact session tokens and owns refunds.
    subtree_sessions: u64,
    session_limit: u64,
}

impl JobSlot {
    pub(super) const fn empty() -> Self {
        Self {
            generation: 1,
            in_use: false,
            parent: NO_PARENT,
            depth: 0,
            state: JobState::Live,
            kills: 0,
            limits: JobLimits::UNSET,
            policy: JobPolicy::ALL,
            subtree_sessions: 0,
            session_limit: LIMIT_UNSET,
        }
    }

    /// The root job, in use from boot.
    pub(super) const fn root() -> Self {
        Self {
            in_use: true,
            ..Self::empty()
        }
    }
}

impl ProcessTable {
    fn job_slot(&self, id: JobId) -> Option<&JobSlot> {
        let slot = self.jobs.get(id.index as usize)?;
        (slot.in_use && slot.generation == id.generation).then_some(slot)
    }

    fn job_id_of(&self, index: u32) -> JobId {
        JobId {
            index,
            generation: self.jobs[index as usize].generation,
        }
    }

    /// `ancestor` is `job` or one of its ancestors.
    fn job_contains(&self, ancestor: u32, job: u32) -> bool {
        let mut cursor = job;
        // The depth bound makes this finite even if a slot were corrupted.
        for _ in 0..=usize::from(JOB_MAX_DEPTH) {
            if cursor == ancestor {
                return true;
            }
            let parent = self.jobs[cursor as usize].parent;
            if parent == NO_PARENT {
                return false;
            }
            cursor = parent;
        }
        false
    }

    fn live_member_count(&self, job: u32, subtree: bool) -> u64 {
        self.slots
            .iter()
            .filter_map(|slot| slot.object.as_deref())
            .filter(|object| !object.exiting)
            .filter(|object| {
                let member = object.job.index;
                if subtree {
                    self.job_contains(job, member)
                } else {
                    member == job
                }
            })
            .count() as u64
    }

    /// Any process object still naming `job`, exiting or not. A slot may be
    /// recycled only when this is false, because an exiting process that has not
    /// been reaped still carries its job index.
    fn has_any_member_object(&self, job: u32) -> bool {
        self.slots
            .iter()
            .filter_map(|slot| slot.object.as_deref())
            .any(|object| object.job.index == job)
    }

    /// This job's own limit in a dimension. The root's process limit is the
    /// system-wide admitted slot count, so there is one source of truth for it.
    fn own_limit(&self, index: u32, threads: bool) -> u64 {
        let slot = &self.jobs[index as usize];
        match (index, threads) {
            (0, false) => super::process_limit() as u64,
            (_, false) => slot.limits.processes,
            (_, true) => slot.limits.threads,
        }
    }

    /// The tightest limit on the chain from `index` to the root.
    fn effective_limit(&self, index: u32, threads: bool) -> u64 {
        let mut cursor = index;
        let mut effective = LIMIT_UNSET;
        for _ in 0..=usize::from(JOB_MAX_DEPTH) {
            effective = effective.min(self.own_limit(cursor, threads));
            let parent = self.jobs[cursor as usize].parent;
            if parent == NO_PARENT {
                break;
            }
            cursor = parent;
        }
        effective
    }

    /// Unreaped process objects in `job`'s subtree, exiting ones included: they
    /// still hold a slot, an address space and their threads until reaped.
    fn subtree_objects(&self, job: u32) -> u64 {
        self.slots
            .iter()
            .filter_map(|slot| slot.object.as_deref())
            .filter(|object| self.job_contains(job, object.job.index))
            .count() as u64
    }

    fn subtree_threads(&self, job: u32) -> u64 {
        self.slots
            .iter()
            .filter_map(|slot| slot.object.as_deref())
            .filter(|object| self.job_contains(job, object.job.index))
            .map(|object| object.thread_count as u64)
            .sum()
    }

    /// Whether one more process, or `extra_threads` more threads, fits under
    /// every limit on the chain. Counts include existing objects only, so the
    /// caller asks before adding.
    fn admits(&self, index: u32, new_processes: u64, new_threads: u64) -> bool {
        let mut cursor = index;
        for _ in 0..=usize::from(JOB_MAX_DEPTH) {
            let processes = self.own_limit(cursor, false);
            if new_processes != 0
                && processes != LIMIT_UNSET
                && self.subtree_objects(cursor).saturating_add(new_processes) > processes
            {
                return false;
            }
            let threads = self.own_limit(cursor, true);
            if new_threads != 0
                && threads != LIMIT_UNSET
                && self.subtree_threads(cursor).saturating_add(new_threads) > threads
            {
                return false;
            }
            let parent = self.jobs[cursor as usize].parent;
            if parent == NO_PARENT {
                break;
            }
            cursor = parent;
        }
        true
    }

    /// A new process (with its first thread) fits in `job`.
    pub(super) fn admits_process(&self, job: JobId) -> bool {
        self.job_slot(job).is_some() && self.admits(job.index, 1, 1)
    }

    /// One more thread fits in the job `index`.
    pub(super) fn admits_thread(&self, index: u32) -> bool {
        self.admits(index, 0, 1)
    }

    fn child_count(&self, job: u32) -> u64 {
        self.jobs
            .iter()
            .enumerate()
            .filter(|(index, slot)| slot.in_use && slot.parent == job && *index as u32 != job)
            .count() as u64
    }

    /// Resolves the job a new process joins: an explicit live job, else the
    /// parent's job, else the root. A `Killing` job refuses, which is how a kill
    /// excludes a fork that races it.
    pub(super) fn resolve_new_member_job(
        &self,
        explicit: Option<JobId>,
        parent_process_id: Option<u64>,
    ) -> Result<JobId, JobError> {
        let id = match explicit {
            Some(id) => id,
            None => parent_process_id
                .and_then(|pid| {
                    self.slots
                        .iter()
                        .filter_map(|slot| slot.object.as_deref())
                        .find(|object| object.process_id == pid)
                        .map(|object| object.job)
                })
                .unwrap_or(ROOT_JOB),
        };
        match self.job_slot(id) {
            None => Err(JobError::Stale),
            Some(slot) if slot.state == JobState::Killing => Err(JobError::Killing),
            Some(_) => Ok(id),
        }
    }

    fn release_dead_jobs(&mut self) {
        // A killing job with no member and no child is dead. Release leaves
        // first: repeating until nothing changes handles a chain in one call.
        loop {
            let dead = (1..JOB_SLOTS).find(|&index| {
                let slot = &self.jobs[index];
                slot.in_use
                    && slot.state == JobState::Killing
                    && self.child_count(index as u32) == 0
                    && !self.has_any_member_object(index as u32)
                    // Pages of an already reaped process may still be draining
                    // out of a retiring address space; the slot is not reusable
                    // while any are charged to it.
                    && kernel_mm::api::charge::usage(index) == 0
                    && slot.subtree_sessions == 0
            });
            let Some(index) = dead else { return };
            policy::invalidate_job(index);
            let slot = &mut self.jobs[index];
            slot.in_use = false;
            slot.parent = NO_PARENT;
            slot.state = JobState::Live;
            // A recycled slot must not inherit its previous tenant's limits.
            slot.limits = JobLimits::UNSET;
            slot.policy = JobPolicy::ALL;
            slot.session_limit = LIMIT_UNSET;
            slot.subtree_sessions = 0;
            // Generation exhaustion retires the slot permanently, exactly like
            // a process slot: reusing generation one would alias a stale id.
            slot.generation = slot.generation.checked_add(1).unwrap_or(0);
        }
    }
}

/// Creates a child job under `parent`.
pub fn job_create_child(parent: JobId) -> Result<JobId, JobError> {
    let mut table = PROCESS_TABLE.lock();
    let parent_slot = *table.job_slot(parent).ok_or(JobError::Stale)?;
    if parent_slot.state == JobState::Killing {
        return Err(JobError::Killing);
    }
    if parent_slot.depth >= JOB_MAX_DEPTH {
        return Err(JobError::TooDeep);
    }
    let index = table
        .jobs
        .iter()
        .position(|slot| !slot.in_use && slot.generation != 0)
        .ok_or(JobError::Full)?;
    let slot = &mut table.jobs[index];
    slot.in_use = true;
    slot.parent = parent.index;
    slot.depth = parent_slot.depth + 1;
    slot.state = JobState::Live;
    slot.kills = 0;
    slot.limits = JobLimits::UNSET;
    slot.policy = JobPolicy::ALL;
    slot.subtree_sessions = 0;
    slot.session_limit = LIMIT_UNSET;
    kernel_mm::api::charge::init_group(index, Some(parent.index as usize));
    let job = table.job_id_of(index as u32);
    policy::publish_job(&table, job);
    Ok(job)
}

pub fn job_info(id: JobId) -> Result<JobInfo, JobError> {
    let table = PROCESS_TABLE.lock();
    let slot = *table.job_slot(id).ok_or(JobError::Stale)?;
    Ok(JobInfo {
        id,
        parent: (slot.parent != NO_PARENT).then(|| table.job_id_of(slot.parent)),
        depth: slot.depth,
        state: slot.state,
        processes: table.live_member_count(id.index, false),
        subtree_processes: table.live_member_count(id.index, true),
        children: table.child_count(id.index),
        kills: slot.kills,
        limits: JobLimits {
            processes: table.own_limit(id.index, false),
            threads: table.own_limit(id.index, true),
            memory_pages: kernel_mm::api::charge::limit(id.index as usize),
        },
        effective: JobLimits {
            processes: table.effective_limit(id.index, false),
            threads: table.effective_limit(id.index, true),
            memory_pages: kernel_mm::api::charge::effective_limit(id.index as usize),
        },
        usage_processes: table.subtree_objects(id.index),
        usage_threads: table.subtree_threads(id.index),
        usage_memory_pages: kernel_mm::api::charge::usage(id.index as usize),
    })
}

/// Whether one more process would fit under every limit on `job`'s chain.
pub fn job_admits_process(job: JobId) -> bool {
    PROCESS_TABLE.lock().admits_process(job)
}

/// Sets the dimensions in `mask` on `id`. A value above what an ancestor
/// allows is accepted and clamped in the effective limit, because refusing it
/// would make the order of operations matter when a parent is later tightened.
/// Lowering a limit below current usage evicts nobody; it only stops admission.
/// The root's process limit is the system-wide slot count, clamped to the slot
/// storage ceiling.
pub fn job_set_limits(id: JobId, mask: LimitMask, limits: JobLimits) -> Result<(), JobError> {
    let mut table = PROCESS_TABLE.lock();
    let slot = table.job_slot(id).ok_or(JobError::Stale)?;
    if slot.state == JobState::Killing {
        return Err(JobError::Killing);
    }
    if mask.contains(LimitMask::PROCESSES) {
        if id.index == 0 {
            super::set_process_limit(limits.processes as usize);
        } else {
            table.jobs[id.index as usize].limits.processes = limits.processes;
        }
    }
    if mask.contains(LimitMask::THREADS) {
        table.jobs[id.index as usize].limits.threads = limits.threads;
    }
    if mask.contains(LimitMask::MEMORY) {
        // The charge group owns the limit so the fault path reads it lock-free.
        kernel_mm::api::charge::set_limit(id.index as usize, limits.memory_pages);
    }
    Ok(())
}

/// Marks `id` and every descendant `Killing`. Idempotent. After it returns no
/// process or job can join the subtree; members are then ended by the caller
/// with `job_collect_members` until none remain.
pub fn job_kill_begin(id: JobId) -> Result<(), JobError> {
    if id == ROOT_JOB {
        return Err(JobError::Root);
    }
    let mut table = PROCESS_TABLE.lock();
    table.job_slot(id).ok_or(JobError::Stale)?;
    for index in 0..JOB_SLOTS {
        if table.jobs[index].in_use && table.job_contains(id.index, index as u32) {
            policy::invalidate_job(index);
            let slot = &mut table.jobs[index];
            if slot.state != JobState::Killing {
                slot.state = JobState::Killing;
                slot.kills = slot.kills.saturating_add(1);
            }
        }
    }
    table.release_dead_jobs();
    Ok(())
}

/// Writes the pids of live members of `id`'s subtree into `out` and returns how
/// many. Stale or released jobs have no members.
pub fn job_collect_members(id: JobId, out: &mut [u64]) -> usize {
    let mut table = PROCESS_TABLE.lock();
    let mut len = 0;
    if table.job_slot(id).is_some() {
        for slot in table.slots.iter() {
            if len == out.len() {
                break;
            }
            let Some(object) = slot.object.as_deref() else { continue };
            if !object.exiting && table.job_contains(id.index, object.job.index) {
                out[len] = object.process_id;
                len += 1;
            }
        }
    }
    table.release_dead_jobs();
    len
}

/// The job `process_id` belongs to, while that process is live.
pub fn process_job(process_id: u64) -> Option<JobId> {
    let table = PROCESS_TABLE.lock();
    table
        .slots
        .iter()
        .filter_map(|slot| slot.object.as_deref())
        .find(|object| object.process_id == process_id)
        .map(|object| object.job)
}

/// True when `process_id` lives in `id`'s subtree.
pub fn job_subtree_contains_process(id: JobId, process_id: u64) -> bool {
    let table = PROCESS_TABLE.lock();
    table.job_slot(id).is_some()
        && table
            .slots
            .iter()
            .filter_map(|slot| slot.object.as_deref())
            .find(|object| object.process_id == process_id)
            .is_some_and(|object| table.job_contains(id.index, object.job.index))
}

/// True when `descendant` is `ancestor` or lies below it, and both are live
/// jobs of the generation named. A stale id on either side is false, so this
/// never distinguishes "recycled" from "not below".
pub fn job_is_within(ancestor: JobId, descendant: JobId) -> bool {
    let table = PROCESS_TABLE.lock();
    table.job_slot(ancestor).is_some()
        && table.job_slot(descendant).is_some()
        && table.job_contains(ancestor.index, descendant.index)
}



/// Kernel-owned memory pressure policy. Memory.max keeps its existing
/// fault-path admission. Soft high emits a bounded counter and protected min
/// only affects the root supervisor's slow OOM victim policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JobMemoryQos {
    pub high: u64,
    pub min: u64,
    pub effective_high: u64,
    pub high_events: u64,
    pub usage: u64,
}

pub fn job_memory_qos(id: JobId) -> Result<JobMemoryQos, JobError> {
    let table = PROCESS_TABLE.lock();
    table.job_slot(id).ok_or(JobError::Stale)?;
    Ok(JobMemoryQos {
        high: kernel_mm::api::charge::high(id.index as usize),
        min: kernel_mm::api::charge::minimum(id.index as usize),
        effective_high: kernel_mm::api::charge::effective_high(id.index as usize),
        high_events: kernel_mm::api::charge::high_events(id.index as usize),
        usage: kernel_mm::api::charge::usage(id.index as usize),
    })
}

/// Publish high/min atomically with respect to the Job tree. No memory
/// reclaim is performed while holding the table lock. Child minima are
/// admitted only into an explicit parent reserve, preventing double grants.
pub fn job_set_memory_qos(id: JobId, high: u64, min: u64) -> Result<JobMemoryQos, JobError> {
    let table = PROCESS_TABLE.lock();
    let slot = table.job_slot(id).ok_or(JobError::Stale)?;
    if slot.state == JobState::Killing {
        return Err(JobError::Killing);
    }
    if high != LIMIT_UNSET && min > high {
        return Err(JobError::Full);
    }
    let hard = kernel_mm::api::charge::limit(id.index as usize);
    if hard != LIMIT_UNSET && min > hard {
        return Err(JobError::Full);
    }
    if slot.parent != NO_PARENT && min != 0 {
        let parent_min = kernel_mm::api::charge::minimum(slot.parent as usize);
        let mut sibling_min = 0_u64;
        for (index, child) in table.jobs.iter().enumerate() {
            if child.in_use && child.parent == slot.parent && index != id.index as usize {
                sibling_min = sibling_min.saturating_add(kernel_mm::api::charge::minimum(index));
            }
        }
        if sibling_min.saturating_add(min) > parent_min {
            return Err(JobError::Full);
        }
    }
    let child_total = table.jobs.iter().enumerate()
        .filter(|(_, child)| child.in_use && child.parent == id.index)
        .fold(0u64, |sum, (index, _)| {
            sum.saturating_add(kernel_mm::api::charge::minimum(index))
        });
    if min < child_total {
        return Err(JobError::Full);
    }
    kernel_mm::api::charge::set_qos(id.index as usize, high, min);
    Ok(JobMemoryQos {
        high,
        min,
        effective_high: kernel_mm::api::charge::effective_high(id.index as usize),
        high_events: kernel_mm::api::charge::high_events(id.index as usize),
        usage: kernel_mm::api::charge::usage(id.index as usize),
    })
}

/// Bounded, generation-aware job session-accounting snapshot. The page count
/// ledger is separate: a session token must never manufacture a mapped page.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JobSessionInfo {
    pub job: JobId,
    pub limit: u64,
    pub effective_limit: u64,
    pub usage: u64,
}

fn session_info_locked(table: &ProcessTable, id: JobId) -> Result<JobSessionInfo, JobError> {
    let slot = table.job_slot(id).ok_or(JobError::Stale)?;
    let mut effective = LIMIT_UNSET;
    let mut cursor = id.index;
    for _ in 0..=usize::from(JOB_MAX_DEPTH) {
        let ancestor = &table.jobs[cursor as usize];
        effective = effective.min(ancestor.session_limit);
        if ancestor.parent == NO_PARENT {
            break;
        }
        cursor = ancestor.parent;
    }
    Ok(JobSessionInfo {
        job: id,
        limit: slot.session_limit,
        effective_limit: effective,
        usage: slot.subtree_sessions,
    })
}

/// Inspect a live job's session capacity, accounting and inherited ceiling.
pub fn job_session_info(id: JobId) -> Result<JobSessionInfo, JobError> {
    session_info_locked(&PROCESS_TABLE.lock(), id)
}

/// Update the soft-independent hard session ceiling for one job. Lowering it
/// refuses future sessions but does not destroy already admitted sessions.
pub fn job_set_session_limit(id: JobId, limit: u64) -> Result<JobSessionInfo, JobError> {
    let mut table = PROCESS_TABLE.lock();
    let slot = table.job_slot(id).ok_or(JobError::Stale)?;
    if slot.state == JobState::Killing {
        return Err(JobError::Killing);
    }
    table.jobs[id.index as usize].session_limit = limit;
    session_info_locked(&table, id)
}

/// Charge one new session to the *actual* client's Job, not to sessiond.
/// Both process membership and admission are checked under the table lock,
/// closing a concurrent exit/reparent/recycled JobId race.
pub fn job_charge_client_session(
    client_pid: u64,
    expected_job: JobId,
) -> Result<JobSessionInfo, JobError> {
    let mut table = PROCESS_TABLE.lock();
    let member = table.slots.iter()
        .filter_map(|slot| slot.object.as_deref())
        .find(|object| object.process_id == client_pid && !object.exiting)
        .map(|object| object.job)
        .ok_or(JobError::Stale)?;
    if member != expected_job {
        return Err(JobError::Stale);
    }
    let job = table.job_slot(expected_job).ok_or(JobError::Stale)?;
    if job.state != JobState::Live {
        return Err(JobError::Killing);
    }
    let mut cursor = expected_job.index;
    // Preflight whole chain before incrementing any counter.
    for _ in 0..=usize::from(JOB_MAX_DEPTH) {
        let slot = &table.jobs[cursor as usize];
        if slot.state != JobState::Live {
            return Err(JobError::Killing);
        }
        if slot.subtree_sessions == u64::MAX
            || (slot.session_limit != LIMIT_UNSET
                && slot.subtree_sessions >= slot.session_limit)
        {
            return Err(JobError::Full);
        }
        if slot.parent == NO_PARENT {
            break;
        }
        cursor = slot.parent;
    }
    let mut cursor = expected_job.index;
    for _ in 0..=usize::from(JOB_MAX_DEPTH) {
        let slot = &mut table.jobs[cursor as usize];
        slot.subtree_sessions += 1;
        if slot.parent == NO_PARENT {
            break;
        }
        cursor = slot.parent;
    }
    session_info_locked(&table, expected_job)
}

/// Exactly one supervisor-owned session token has been retired. Callers must
/// retain the original JobId generation until release; the Job slot cannot be
/// recycled while its session counter is nonzero.
pub fn job_release_client_session(job: JobId) -> Result<JobSessionInfo, JobError> {
    let mut table = PROCESS_TABLE.lock();
    let slot = table.job_slot(job).ok_or(JobError::Stale)?;
    if slot.subtree_sessions == 0 {
        return Err(JobError::Stale);
    }
    // Never permit partial subtraction if an ancestor ledger is corrupt.
    let mut cursor = job.index;
    for _ in 0..=usize::from(JOB_MAX_DEPTH) {
        let slot = &table.jobs[cursor as usize];
        if slot.subtree_sessions == 0 {
            return Err(JobError::Stale);
        }
        if slot.parent == NO_PARENT {
            break;
        }
        cursor = slot.parent;
    }
    let mut cursor = job.index;
    for _ in 0..=usize::from(JOB_MAX_DEPTH) {
        let slot = &mut table.jobs[cursor as usize];
        slot.subtree_sessions -= 1;
        if slot.parent == NO_PARENT {
            break;
        }
        cursor = slot.parent;
    }
    // A killing job is eligible for recycling once the last exact charge
    // and all its members/pages have drained.
    let info = session_info_locked(&table, job)?;
    table.release_dead_jobs();
    Ok(info)
}

#[cfg(test)]
pub(super) fn reset_policy_for_tests() { policy::reset_for_tests(); }

#[cfg(test)]
mod tests;
