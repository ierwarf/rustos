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

/// Slab capacity of the job table. An internal bound of this implementation, not
/// an ABI value: it moves into the root job's `jobs.max` policy when the process
/// table gets its directory (see `docs/architecture/job-model.md`, phase 2a).
pub(super) const JOB_SLOTS: usize = 64;
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
    pub const fn from_raw(raw: u64) -> Self {
        Self {
            index: (raw >> 32) as u32,
            generation: raw as u32,
        }
    }

    pub const fn to_raw(self) -> u64 {
        ((self.index as u64) << 32) | self.generation as u64
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
}

#[derive(Clone, Copy)]
pub(super) struct JobSlot {
    generation: u32,
    in_use: bool,
    parent: u32,
    depth: u8,
    state: JobState,
    kills: u64,
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
            });
            let Some(index) = dead else { return };
            let slot = &mut self.jobs[index];
            slot.in_use = false;
            slot.parent = NO_PARENT;
            slot.state = JobState::Live;
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
    Ok(table.job_id_of(index as u32))
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
    })
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

#[cfg(test)]
mod tests;
