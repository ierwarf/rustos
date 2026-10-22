//! Owner: one-shot BSP/AP scheduler image, exact lifecycle and initial bank producers.
//! Boundary: boot mechanism only; not a runtime directory, policy or hardware ACK.
//! Lifecycle: Vacant -> Building -> Ready; abandonment poisons, never restarts boot.
//! Concurrency: BSP non-Send custody, unique per-CPU publication before bank effects.
//! Failure: partial images/pins remain quarantined; no implicit teardown or retry.
//! Forbidden: no global scheduler guard, live reset, generation alias or ready prefix.
//! Evidence: bounded native authority witnesses and compiler/source routing; no target proof.
use super::{MAX_TASK, ROOT_TASK_SLOT, runqueue};
mod initial_tasks;
use core::{
    cell::Cell,
    marker::PhantomData,
    sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering},
};
use kernel_hal::api::cpu::{self, CpuLifecycleState};
const CAP: usize = nucleus_core::util::lockdep::MAX_TRACKED_CPUS;
const VACANT: u8 = 0;
const BUILDING: u8 = 1;
const READY: u8 = 2;
const POISON: u8 = 3;
const RESERVED: u8 = 1;
const EFFECTS: u8 = 2;
const PUBLISHING: u8 = 3;
const PUBLISHED: u8 = 4;
const ENTERING: u8 = 5;
const ONLINE: u8 = 6;
const CPU_POISON: u8 = 7;
struct CpuCell {
    phase: AtomicU8,
    generation: AtomicU64,
    slot: AtomicUsize,
    id: AtomicU64,
    owner: AtomicU64,
}
impl CpuCell {
    const fn new() -> Self {
        Self {
            phase: AtomicU8::new(0),
            generation: AtomicU64::new(0),
            slot: AtomicUsize::new(0),
            id: AtomicU64::new(0),
            owner: AtomicU64::new(0),
        }
    }
}
struct ImageCell {
    phase: AtomicU8,
    count: AtomicUsize,
    cpus: [CpuCell; CAP],
}
impl ImageCell {
    const fn new() -> Self {
        Self {
            phase: AtomicU8::new(VACANT),
            count: AtomicUsize::new(0),
            cpus: [const { CpuCell::new() }; CAP],
        }
    }
    fn claim(&self, generations: &[u64]) -> Option<ImageKey<'_>> {
        if generations.is_empty() || generations.len() > CAP || generations.iter().any(|g| *g == 0)
        {
            return None;
        }
        self.phase
            // ORDERING: Single-winner phase admission precedes effects; successful Release carries prior tuple/image writes.
            .compare_exchange(VACANT, BUILDING, Ordering::AcqRel, Ordering::Acquire)
            .ok()?;
        self.count.store(generations.len(), Ordering::Relaxed);
        for (cell, generation) in self.cpus.iter().zip(generations) {
            cell.generation.store(*generation, Ordering::Relaxed);
        }
        Some(ImageKey {
            cell: self,
            live: true,
            affinity: PhantomData,
        })
    }
    fn entry(&self, cpu: usize, generation: u64) -> Option<Entry<'_>> {
        // ORDERING: Acquire observes producer admission before consuming its immutable tuple or published Current.
        if self.phase.load(Ordering::Acquire) != READY
            || cpu == 0
            || cpu >= self.count.load(Ordering::Relaxed)
        {
            return None;
        }
        let cell = &self.cpus[cpu];
        if generation == 0 || cell.generation.load(Ordering::Relaxed) != generation {
            return None;
        }
        cell.phase
            // ORDERING: Single-winner phase admission precedes effects; successful Release carries prior tuple/image writes.
            .compare_exchange(PUBLISHED, ENTERING, Ordering::AcqRel, Ordering::Acquire)
            .ok()?;
        Some(Entry {
            image: self,
            cpu,
            generation,
            live: true,
            affinity: PhantomData,
        })
    }
}
static IMAGE: ImageCell = ImageCell::new();
pub(in crate::multitask) struct ImageKey<'a> {
    cell: &'a ImageCell,
    live: bool,
    affinity: PhantomData<*mut ()>,
}
impl<'a> ImageKey<'a> {
    fn cpu(&self, cpu: usize, id: u64) -> Option<InitialCpu<'_>> {
        // ORDERING: Acquire observes producer admission before consuming its immutable tuple or published Current.
        if self.cell.phase.load(Ordering::Acquire) != BUILDING
            || cpu >= self.cell.count.load(Ordering::Relaxed)
            || (cpu == 0 && id != 0)
            || (cpu != 0 && id == 0)
        {
            return None;
        }
        let slot = if cpu == 0 {
            ROOT_TASK_SLOT
        } else {
            MAX_TASK.checked_sub(cpu)?
        };
        // The physical CPU ledger must not alias an already captured task UID.
        if self.cell.cpus.iter().enumerate().any(|(other, c)| {
            other != cpu
                // ORDERING: Acquire observes producer admission before consuming its immutable tuple or published Current.
                && c.phase.load(Ordering::Acquire) != 0
                && c.id.load(Ordering::Relaxed) == id
        }) {
            return None;
        }
        let cell = &self.cell.cpus[cpu];
        cell.phase
            // ORDERING: Single-winner phase admission precedes effects; successful Release carries prior tuple/image writes.
            .compare_exchange(0, RESERVED, Ordering::AcqRel, Ordering::Acquire)
            .ok()?;
        cell.slot.store(slot, Ordering::Relaxed);
        cell.id.store(id, Ordering::Relaxed);
        Some(InitialCpu {
            image: self.cell,
            cpu,
            generation: cell.generation.load(Ordering::Relaxed),
            slot,
            id,
            live: Cell::new(true),
            affinity: PhantomData,
        })
    }
    fn hardware(&self) -> HardwareScope<'_> {
        HardwareScope {
            image: self.cell,
            cpu: 0,
            phase: PUBLISHED,
        }
    }
    fn seal(&mut self) -> Option<ReadyImage<'_>> {
        let count = self.cell.count.load(Ordering::Relaxed);
        if self.cell.cpus[..count]
            .iter()
            // ORDERING: Acquire observes producer admission before consuming its immutable tuple or published Current.
            .any(|c| c.phase.load(Ordering::Acquire) != PUBLISHED)
        {
            return None;
        }
        self.cell
            .phase
            // ORDERING: Single-winner phase admission precedes effects; successful Release carries prior tuple/image writes.
            .compare_exchange(BUILDING, READY, Ordering::Release, Ordering::Acquire)
            .ok()?;
        self.live = false;
        Some(ReadyImage { image: self.cell })
    }
}
impl Drop for ImageKey<'_> {
    fn drop(&mut self) {
        if self.live {
            // ORDERING: Release publishes irreversible phase/poison after owner effects; it never acknowledges hardware.
            self.cell.phase.store(POISON, Ordering::Release);
        }
    }
}
pub(in crate::multitask) struct InitialCpu<'a> {
    image: &'a ImageCell,
    cpu: usize,
    generation: u64,
    slot: usize,
    id: u64,
    live: Cell<bool>,
    affinity: PhantomData<*mut ()>,
}
impl InitialCpu<'_> {
    pub(in crate::multitask) fn cpu(&self) -> usize {
        self.cpu
    }
    pub(in crate::multitask) fn slot(&self) -> usize {
        self.slot
    }
    pub(in crate::multitask) fn id(&self) -> u64 {
        self.id
    }
    fn generation(&self) -> u64 {
        self.generation
    }
    fn check(&self, phase: u8) {
        assert!(
            self.live.get()
                // ORDERING: Acquire observes producer admission before consuming its immutable tuple or published Current.
                && self.image.phase.load(Ordering::Acquire) == BUILDING
                && self.image.cpus[self.cpu].phase.load(Ordering::Acquire) == phase
                && self.image.cpus[self.cpu].generation.load(Ordering::Relaxed) == self.generation
                && self.image.cpus[self.cpu].slot.load(Ordering::Relaxed) == self.slot
                && self.image.cpus[self.cpu].id.load(Ordering::Relaxed) == self.id,
            "bootstrap lost exact initial producer"
        );
    }
    pub(super) fn begin_effects(&self) {
        self.check(RESERVED);
        self.image.cpus[self.cpu]
            .phase
            // ORDERING: Release publishes irreversible phase/poison after owner effects; it never acknowledges hardware.
            .store(EFFECTS, Ordering::Release);
    }
    pub(super) fn assert_materializing(&self) {
        self.check(EFFECTS);
    }
    pub(super) fn assert_effects(&self) {
        self.assert_materializing();
        assert_parked(self.cpu, self.generation());
    }
    pub(super) fn ap_stack(&self) -> (u64, u64) {
        self.assert_effects();
        assert_ne!(self.cpu, 0);
        cpu::ap_bootstrap_stack_bounds(self.cpu as u8, self.generation())
    }
    pub(super) fn publish_current(&self) {
        self.assert_effects();
        let owner = runqueue::owner(self.slot());
        assert!(
            owner.state == runqueue::RunOwnerState::Running
                && owner.cpu == Some(self.cpu)
                && owner.generation != 0,
            "initial bank lacks exact Running owner"
        );
        self.image.cpus[self.cpu]
            .owner
            .store(owner.generation, Ordering::Relaxed);
        self.begin_publication(owner.generation)
            .expect("initial bank producer spent twice");
        crate::multitask::cpu_local::publish_cpu_current_task(self);
        self.complete_publication();
    }
    fn begin_publication(&self, owner: u64) -> Option<()> {
        if owner == 0
            || !self.live.get()
            // ORDERING: Acquire observes producer admission before consuming its immutable tuple or published Current.
            || self.image.phase.load(Ordering::Acquire) != BUILDING
            || self.image.cpus[self.cpu].generation.load(Ordering::Relaxed) != self.generation
            || self.image.cpus[self.cpu].slot.load(Ordering::Relaxed) != self.slot
            || self.image.cpus[self.cpu].id.load(Ordering::Relaxed) != self.id
        {
            return None;
        }
        self.image.cpus[self.cpu]
            .phase
            // ORDERING: Single-winner phase admission precedes effects; successful Release carries prior tuple/image writes.
            .compare_exchange(EFFECTS, PUBLISHING, Ordering::AcqRel, Ordering::Acquire)
            .ok()?;
        Some(())
    }
    pub(in crate::multitask) fn assert_publication(&self) {
        self.check(PUBLISHING);
        assert_parked(self.cpu, self.generation());
        let owner = runqueue::owner(self.slot());
        assert!(
            owner.state == runqueue::RunOwnerState::Running
                && owner.cpu == Some(self.cpu)
                && owner.generation == self.image.cpus[self.cpu].owner.load(Ordering::Relaxed),
            "initial publication lost Running generation"
        );
    }
    fn complete_publication(&self) {
        self.check(PUBLISHING);
        self.image.cpus[self.cpu]
            .phase
            // ORDERING: Single-winner phase admission precedes effects; successful Release carries prior tuple/image writes.
            .compare_exchange(PUBLISHING, PUBLISHED, Ordering::Release, Ordering::Acquire)
            .expect("initial publication lost producer ACK");
        self.live.set(false);
    }
}
impl Drop for InitialCpu<'_> {
    fn drop(&mut self) {
        if self.live.get() {
            self.image.cpus[self.cpu]
                .phase
                // ORDERING: Release publishes irreversible phase/poison after owner effects; it never acknowledges hardware.
                .store(CPU_POISON, Ordering::Release);
            self.image.phase.store(POISON, Ordering::Release);
        }
    }
}
pub(in crate::multitask) struct ReadyImage<'a> {
    image: &'a ImageCell,
}
impl ReadyImage<'_> {
    pub(in crate::multitask) fn assert_complete(&self) {
        assert_eq!(
            // ORDERING: Acquire observes producer admission before consuming its immutable tuple or published Current.
            self.image.phase.load(Ordering::Acquire),
            READY,
            "scheduler ready lacks complete image"
        );
        assert!(
            self.image.cpus[..self.image.count.load(Ordering::Relaxed)]
                .iter()
                // ORDERING: Acquire observes producer admission before consuming its immutable tuple or published Current.
                .all(|c| c.phase.load(Ordering::Acquire) >= PUBLISHED
                    && c.phase.load(Ordering::Acquire) < CPU_POISON)
        );
    }
}
pub(in crate::multitask) struct Entry<'a> {
    image: &'a ImageCell,
    cpu: usize,
    generation: u64,
    live: bool,
    affinity: PhantomData<*mut ()>,
}
impl Entry<'_> {
    #[cfg(test)]
    pub(in crate::multitask) fn cpu(&self) -> usize {
        self.cpu
    }
    pub(in crate::multitask) fn slot(&self) -> usize {
        self.image.cpus[self.cpu].slot.load(Ordering::Relaxed)
    }
    pub(super) fn assert_entering(&self) {
        assert!(
            self.live
                // ORDERING: Acquire observes producer admission before consuming its immutable tuple or published Current.
                && self.image.phase.load(Ordering::Acquire) == READY
                && self.image.cpus[self.cpu].phase.load(Ordering::Acquire) == ENTERING,
            "AP entry lost exact producer"
        );
        let snapshot = cpu::lifecycle_snapshot(self.cpu as u8).expect("AP entry lacks lifecycle");
        assert!(
            snapshot.generation == self.generation
                && snapshot.state == CpuLifecycleState::SchedulerReady,
            "AP entry lost lifecycle generation"
        );
        let owner = runqueue::owner(self.slot());
        assert!(
            owner.state == runqueue::RunOwnerState::Running
                && owner.cpu == Some(self.cpu)
                && owner.generation == self.image.cpus[self.cpu].owner.load(Ordering::Relaxed),
            "AP entry lost initial Running generation"
        );
        assert_eq!(
            crate::multitask::current_identity::read_dispatch(self.slot())
                .and_then(|id| id.task_id),
            Some(self.image.cpus[self.cpu].id.load(Ordering::Relaxed)),
            "AP entry lost captured UID"
        );
    }
    fn hardware(&self) -> HardwareScope<'_> {
        HardwareScope {
            image: self.image,
            cpu: self.cpu,
            phase: ENTERING,
        }
    }
    fn complete(mut self) {
        self.assert_entering();
        cpu::transition_lifecycle(self.cpu as u8, self.generation, CpuLifecycleState::Online);
        self.image.cpus[self.cpu]
            .phase
            // ORDERING: Single-winner phase admission precedes effects; successful Release carries prior tuple/image writes.
            .compare_exchange(ENTERING, ONLINE, Ordering::Release, Ordering::Acquire)
            .expect("AP completion lost producer");
        self.live = false;
    }
}
impl Drop for Entry<'_> {
    fn drop(&mut self) {
        if self.live {
            self.image.cpus[self.cpu]
                .phase
                // ORDERING: Release publishes irreversible phase/poison after owner effects; it never acknowledges hardware.
                .store(CPU_POISON, Ordering::Release);
        }
    }
}
fn assert_parked(index: usize, generation: u64) {
    let snapshot = cpu::lifecycle_snapshot(index as u8).expect("bootstrap lacks CPU lifecycle");
    assert!(
        snapshot.generation == generation && snapshot.state == CpuLifecycleState::OnlineParked,
        "bootstrap lost parked CPU generation"
    );
}
pub(in crate::multitask) struct HardwareScope<'a> {
    image: &'a ImageCell,
    cpu: usize,
    phase: u8,
}
impl HardwareScope<'_> {
    pub(in crate::multitask) fn slot(&self) -> usize {
        self.image.cpus[self.cpu].slot.load(Ordering::Relaxed)
    }
    pub(in crate::multitask) fn assert_current(&self, cpu: usize) {
        assert_eq!(cpu, self.cpu, "bootstrap hardware moved CPUs");
        let root_phase = if cpu == 0 { BUILDING } else { READY };
        assert!(
            // ORDERING: Acquire observes producer admission before consuming its immutable tuple or published Current.
            self.image.phase.load(Ordering::Acquire) == root_phase
                && self.image.cpus[cpu].phase.load(Ordering::Acquire) == self.phase,
            "bootstrap hardware lost image scope"
        );
        assert_eq!(
            crate::multitask::cpu_local::published_current_slot(cpu),
            Some(self.slot()),
            "bootstrap hardware lost initial Current"
        );
    }
}
/// This facade is qualified by the one-shot image key, never runtime authority.
pub(in crate::multitask) struct Bootstrap {
    key: ImageKey<'static>,
    preemption: Option<nucleus_core::util::lockdep::DispatchPreemption<'static>>,
}
impl Bootstrap {
    pub(in crate::multitask) fn begin() -> Self {
        let mut preemption = nucleus_core::util::lockdep::DispatchPreemption::enter();
        preemption.mask_irqs();
        assert_eq!(preemption.cpu(), 0, "bootstrap executed on AP");
        assert!(
            !crate::multitask::cpu_local::scheduler_initialized(),
            "live scheduler cannot reset bootstrap"
        );
        crate::multitask::cpu_local::assert_initial_image_absent();
        let count = cpu::discovered_count();
        assert!(
            (1..=CAP).contains(&count),
            "bootstrap CPU count outside capacity"
        );
        let mut generations = [0; CAP];
        for (index, generation) in generations[..count].iter_mut().enumerate() {
            let snapshot = cpu::lifecycle_snapshot(index as u8).expect("bootstrap absent CPU");
            assert_eq!(snapshot.state, CpuLifecycleState::OnlineParked);
            *generation = snapshot.generation;
        }
        let key = IMAGE
            .claim(&generations[..count])
            .expect("bootstrap image already claimed or poisoned");
        Self {
            key,
            preemption: Some(preemption),
        }
    }
    #[allow(clippy::too_many_arguments)]
    pub(in crate::multitask) fn reset_root(
        &mut self,
        pit: u16,
        entry: fn(u64),
        cs: u64,
        ss: u64,
        flags: u64,
        rip: u64,
    ) {
        let image = self.key.cpu(0, 0).expect("BSP producer unavailable");
        image.begin_effects();
        initial_tasks::initialize_owners(&image, pit);
        initial_tasks::root(&image, pit, entry, cs, ss, flags, rip);
    }
    pub(in crate::multitask) fn initialize_ap(&mut self, index: u8, entry: fn(u64), id: u64) {
        let image = self
            .key
            .cpu(index as usize, id)
            .expect("AP producer unavailable");
        image.begin_effects();
        initial_tasks::idle(&image, entry);
    }
    pub(in crate::multitask) fn prepare_and_publish(mut self) -> usize {
        let preemption = self.preemption.take().expect("bootstrap lost CPU custody");
        let hardware = self.key.hardware();
        let rsp = crate::multitask::cpu_local::with_bootstrap_turn(
            preemption,
            &hardware,
            false,
            |turn| {
                initial_tasks::validate_root_frame(turn);
                super::return_architecture::InitialArchitecture::capture(&hardware, turn)
                    .expect("BSP architecture lost original image/Current")
                    .install();
                super::runqueue::saved_rsp(hardware.slot())
            },
        );
        let count = self.key.cell.count.load(Ordering::Relaxed);
        for index in 0..count {
            assert_parked(
                index,
                self.key.cell.cpus[index].generation.load(Ordering::Relaxed),
            );
        }
        let ready = self
            .key
            .seal()
            .expect("scheduler image has incomplete CPU publication");
        crate::multitask::cpu_local::publish_scheduler_initialized(&ready);
        for index in 0..count {
            let generation = ready.image.cpus[index].generation.load(Ordering::Relaxed);
            cpu::transition_lifecycle(index as u8, generation, CpuLifecycleState::SchedulerReady);
            crate::debug::record_milestone(
                crate::debug::LogCategory::Boot,
                "smp-cpu-scheduler-ready",
                index as u64,
                generation,
            );
        }
        crate::arch::tlb::admit_current_cpu_online();
        cpu::transition_lifecycle(
            0,
            ready.image.cpus[0].generation.load(Ordering::Relaxed),
            CpuLifecycleState::Online,
        );
        crate::debug::record_milestone(
            crate::debug::LogCategory::Boot,
            "smp-cpu-online",
            0,
            ready.image.cpus[0].generation.load(Ordering::Relaxed),
        );
        rsp
    }
}
pub(in crate::multitask) fn enter_secondary() {
    let mut preemption = nucleus_core::util::lockdep::DispatchPreemption::enter();
    preemption.mask_irqs();
    let index = preemption.cpu();
    assert!(index > 0 && index < CAP, "AP entry executed on invalid CPU");
    let snapshot = cpu::lifecycle_snapshot(index as u8).expect("AP entry absent CPU");
    assert_eq!(snapshot.state, CpuLifecycleState::SchedulerReady);
    let entry = IMAGE
        .entry(index, snapshot.generation)
        .expect("AP entry stale, duplicate or poisoned");
    entry.assert_entering();
    let hardware = entry.hardware();
    crate::multitask::cpu_local::with_bootstrap_turn(preemption, &hardware, true, |turn| {
        super::return_architecture::InitialArchitecture::capture(&hardware, turn)
            .expect("AP architecture lost original image/Current")
            .install();
        let id = crate::multitask::current_identity::read_dispatch(entry.slot())
            .and_then(|p| p.task_id).expect("AP entry lost original UID");
        nucleus_core::util::lockdep::set_current_task_owner(
            id.checked_add(1).expect("AP lock owner token exhausted"));
    });
    crate::arch::tlb::admit_current_cpu_online();
    assert!(
        crate::arch::timer::init_current_cpu(),
        "AP clockevent unavailable"
    );
    let generation = entry.generation;
    entry.complete();
    for (category, event) in [
        (crate::debug::LogCategory::Boot, "smp-ap-online"),
        (crate::debug::LogCategory::Boot, "smp-cpu-online"),
        (crate::debug::LogCategory::Sched, "smp-cpu-idle-enter"),
    ] {
        crate::debug::record_milestone(category, event, index as u64, generation);
    }
}
#[cfg(test)]
pub(super) fn initial_interval_witness() {
    initial_tasks::tests::born_initial(0, |image, backing| {
        use super::execution_interval::{ConsumedInterval, consume};
        let ConsumedInterval::Active(interval) = consume(0, image.slot(), image.id()) else {
            panic!("bootstrap lacks original execution custody");
        };
        assert_eq!(interval.elapsed_ns(1, None, |_, _| 100), 100);
        assert!(interval.charge(100, 100).is_none());
        assert!(matches!(consume(0, image.slot(), image.id()), ConsumedInterval::AlreadySettled));
        drop(backing);
    });
}

#[cfg(test)]
mod tests;
