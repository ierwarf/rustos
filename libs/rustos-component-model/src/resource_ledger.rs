//! Allocation-custody planning ledger for Ring3 control decisions.
//!
//! This model never supersedes ring0's exact Job counters. Each real allocation
//! site must install a charge in kernel custody before publishing its object,
//! and undo that charge on every release path. The supervisor uses this ledger
//! to propose budgets and to reconcile independently observed kernel evidence.
use core::cmp::max;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AllocationClass {
    ProcessObject, ThreadObject, PageTable, HandleTable,
    IpcEndpoint, IpcReply, VmaMetadata, SchedulerNode,
    KernelBuffer, DmaPinned,
}
impl AllocationClass {
    pub const COUNT: usize = 10;
    pub const ALL: [Self; Self::COUNT] = [
        Self::ProcessObject, Self::ThreadObject, Self::PageTable,
        Self::HandleTable, Self::IpcEndpoint, Self::IpcReply,
        Self::VmaMetadata, Self::SchedulerNode, Self::KernelBuffer,
        Self::DmaPinned
    ];
    const fn index(self) -> usize { self as usize }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JobKey { pub slot: u32, pub generation: u32 }
impl JobKey {
    pub const fn valid(self) -> bool { self.generation != 0 }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Budget {
    pub max_bytes: u64, pub high_bytes: u64, pub min_bytes: u64,
}
impl Budget {
    pub const fn valid(self) -> bool {
        self.min_bytes <= self.max_bytes && self.high_bytes <= self.max_bytes
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AllocationKey { pub id: u64, pub generation: u64 }
impl AllocationKey { pub fn valid(self) -> bool { self.id != 0 && self.generation != 0 } }
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Invalid, MissingJob, DuplicateJob, ParentMissing, Cycle,
    Capacity, Overflow, Quota, Stale, DuplicateCharge, ForeignRelease,
    Nonempty, EvidenceMismatch,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Job {
    key: JobKey, parent: Option<JobKey>, budget: Budget,
    charged: u64, high_events: u64, by_class: [u64; AllocationClass::COUNT],
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Allocation {
    key: AllocationKey, owner: JobKey, class: AllocationClass, bytes: u64,
}
/// Bounded and allocation-free. This ledger is for control-plane staging and
/// exact reconciliation; its checks do not authorize kernel allocations.
pub struct ResourceLedger<const JOBS: usize, const ALLOCS: usize> {
    jobs: [Option<Job>; JOBS], allocations: [Option<Allocation>; ALLOCS],
    revision: u64,
}
impl<const JOBS: usize, const ALLOCS: usize> ResourceLedger<JOBS, ALLOCS> {
    pub const fn new() -> Self {
        Self { jobs: [None; JOBS], allocations: [None; ALLOCS], revision: 0 }
    }
    pub fn revision(&self) -> u64 { self.revision }
    fn position(&self, key: JobKey) -> Result<usize, Error> {
        if !key.valid() { return Err(Error::Invalid); }
        self.jobs.iter().position(|e| e.is_some_and(|v| v.key == key))
            .ok_or(Error::MissingJob)
    }
    fn ancestor_chain(&self, owner: JobKey) -> Result<([usize; JOBS], usize), Error> {
        let mut chain = [0usize; JOBS];
        let mut length = 0usize;
        let mut current = Some(owner);
        while let Some(job) = current {
            if length == JOBS { return Err(Error::Cycle); }
            let index = self.position(job)?;
            if chain[..length].contains(&index) { return Err(Error::Cycle); }
            chain[length] = index;
            length += 1;
            current = self.jobs[index].ok_or(Error::MissingJob)?.parent;
        }
        Ok((chain, length))
    }
    pub fn insert_job(&mut self, key: JobKey, parent: Option<JobKey>, budget: Budget)
        -> Result<(), Error> {
        if !key.valid() || !budget.valid() || parent == Some(key) {
            return Err(Error::Invalid);
        }
        if self.jobs.iter().flatten().any(|j| j.key.slot == key.slot) {
            return Err(Error::DuplicateJob);
        }
        if let Some(p) = parent {
            self.position(p).map_err(|_| Error::ParentMissing)?;
        }
        let slot = self.jobs.iter().position(Option::is_none).ok_or(Error::Capacity)?;
        self.jobs[slot] = Some(Job {
            key, parent, budget, charged: 0, high_events: 0,
            by_class: [0; AllocationClass::COUNT],
        });
        self.revision = self.revision.checked_add(1).ok_or(Error::Overflow)?;
        Ok(())
    }
    pub fn set_budget(&mut self, key: JobKey, budget: Budget) -> Result<(), Error> {
        if !budget.valid() { return Err(Error::Invalid); }
        let index = self.position(key)?;
        if self.jobs[index].ok_or(Error::MissingJob)?.charged > budget.max_bytes {
            return Err(Error::Quota);
        }
        self.jobs[index].as_mut().ok_or(Error::MissingJob)?.budget = budget;
        self.revision = self.revision.checked_add(1).ok_or(Error::Overflow)?;
        Ok(())
    }
    pub fn preview_charge(&self, owner: JobKey, bytes: u64) -> Result<(), Error> {
        if bytes == 0 { return Err(Error::Invalid); }
        let (chain, len) = self.ancestor_chain(owner)?;
        for &index in &chain[..len] {
            let job = self.jobs[index].ok_or(Error::MissingJob)?;
            let next = job.charged.checked_add(bytes).ok_or(Error::Overflow)?;
            if next > job.budget.max_bytes { return Err(Error::Quota); }
            if job.high_events == u64::MAX && job.charged <= job.budget.high_bytes
                && next > job.budget.high_bytes { return Err(Error::Overflow); }
        }
        Ok(())
    }
    pub fn record_kernel_charge(&mut self, key: AllocationKey, owner: JobKey,
                                class: AllocationClass, bytes: u64) -> Result<(), Error> {
        if !key.valid() || self.allocations.iter().flatten().any(|a| a.key == key) {
            return Err(Error::DuplicateCharge);
        }
        let empty = self.allocations.iter().position(Option::is_none).ok_or(Error::Capacity)?;
        self.preview_charge(owner, bytes)?;
        let (chain, len) = self.ancestor_chain(owner)?;
        // Preflight every class counter and revision before changing any
        // ancestor. An overflow must never leave a partially debited tree.
        self.revision.checked_add(1).ok_or(Error::Overflow)?;
        for &index in &chain[..len] {
            let job = self.jobs[index].ok_or(Error::MissingJob)?;
            job.by_class[class.index()].checked_add(bytes).ok_or(Error::Overflow)?;
        }
        for &index in &chain[..len] {
            let job = self.jobs[index].as_mut().ok_or(Error::MissingJob)?;
            let next = job.charged + bytes;
            if job.charged <= job.budget.high_bytes && next > job.budget.high_bytes {
                job.high_events += 1;
            }
            job.charged = next;
            job.by_class[class.index()] = job.by_class[class.index()]
                .checked_add(bytes).ok_or(Error::Overflow)?;
        }
        self.allocations[empty] = Some(Allocation { key, owner, class, bytes });
        self.revision = self.revision.checked_add(1).ok_or(Error::Overflow)?;
        Ok(())
    }
    pub fn record_kernel_release(&mut self, key: AllocationKey, owner: JobKey)
        -> Result<(), Error> {
        let index = self.allocations.iter().position(|a| a.is_some_and(|v| v.key == key))
            .ok_or(Error::Stale)?;
        let allocation = self.allocations[index].ok_or(Error::Stale)?;
        if allocation.owner != owner { return Err(Error::ForeignRelease); }
        let (chain, len) = self.ancestor_chain(owner)?;
        for &job_index in &chain[..len] {
            let job = self.jobs[job_index].ok_or(Error::MissingJob)?;
            if job.charged < allocation.bytes
                || job.by_class[allocation.class.index()] < allocation.bytes {
                return Err(Error::EvidenceMismatch);
            }
        }
        for &job_index in &chain[..len] {
            let job = self.jobs[job_index].as_mut().ok_or(Error::MissingJob)?;
            job.charged -= allocation.bytes;
            job.by_class[allocation.class.index()] -= allocation.bytes;
        }
        self.allocations[index] = None;
        self.revision = self.revision.checked_add(1).ok_or(Error::Overflow)?;
        Ok(())
    }
    pub fn usage(&self, key: JobKey) -> Result<(u64,u64),Error> {
        let job = self.jobs[self.position(key)?].ok_or(Error::MissingJob)?;
        Ok((job.charged,job.high_events))
    }
    pub fn by_class(&self, key: JobKey, class: AllocationClass)->Result<u64, Error> {
        Ok(self.jobs[self.position(key)?].ok_or(Error::MissingJob)?.by_class[class.index()])
    }
    pub fn remove_job(&mut self, key: JobKey) -> Result<(), Error> {
        let slot = self.position(key)?;
        if self.jobs[slot].ok_or(Error::MissingJob)?.charged != 0
           || self.jobs.iter().flatten().any(|j|j.parent==Some(key)) {
            return Err(Error::Nonempty);
        }
        self.jobs[slot] = None;
        self.revision = self.revision.checked_add(1).ok_or(Error::Overflow)?;
        Ok(())
    }
    pub fn projected_headroom(&self,key:JobKey)->Result<u64,Error> {
        let (chain,len) = self.ancestor_chain(key)?;
        let mut result=u64::MAX;
        for &i in &chain[..len] {
            let j = self.jobs[i].ok_or(Error::MissingJob)?;
            result = core::cmp::min(result,j.budget.max_bytes-j.charged);
        }
        Ok(result)
    }
    pub fn protected_floor(&self, key:JobKey)->Result<u64,Error> {
        let (chain,len)=self.ancestor_chain(key)?;
        let mut floor=0;
        for &i in &chain[..len] {
            let j=self.jobs[i].ok_or(Error::MissingJob)?;
            floor=max(floor,j.budget.min_bytes);
        }
        Ok(floor)
    }
}
impl<const J:usize,const A:usize> Default for ResourceLedger<J,A> {
    fn default()->Self { Self::new() }
}
#[cfg(test)]
mod tests {
    use super::*;
    const B:Budget=Budget{max_bytes:100,high_bytes:80,min_bytes:10};
    #[test]
    fn charges_all_ancestors_and_rejects_unfunded_sibling() {
        let mut l=ResourceLedger::<4,4>::new();
        let root=JobKey{slot:1,generation:1};
        let child=JobKey{slot:2,generation:8};
        l.insert_job(root,None,B).unwrap();
        l.insert_job(child,Some(root),B).unwrap();
        let k=AllocationKey{id:40,generation:3};
        l.record_kernel_charge(k,child,AllocationClass::IpcEndpoint,75).unwrap();
        assert_eq!(l.preview_charge(child,30),Err(Error::Quota));
        assert_eq!(l.record_kernel_release(k,root),Err(Error::ForeignRelease));
        l.record_kernel_release(k,child).unwrap();
        assert_eq!(l.usage(root).unwrap().0,0);
    }
}
