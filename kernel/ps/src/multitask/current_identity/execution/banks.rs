//! CPU prepared/current/outgoing execution permits. Atomic slots move unique
//! RAII units; remote readers see only membership, never mutable permits.
use super::*;
use core::sync::atomic::AtomicUsize;
struct Banks {
    prepared: AtomicUsize,
    current: AtomicUsize,
    outgoing: AtomicUsize,
}
impl Banks {
    const fn new() -> Self {
        Self {
            prepared: AtomicUsize::new(0),
            current: AtomicUsize::new(0),
            outgoing: AtomicUsize::new(0),
        }
    }
    fn put(cell: &AtomicUsize, pin: ExecutionPermit) -> Option<ExecutionPermit> {
        ExecutionPermit::take(cell.swap(pin.park(), Ordering::AcqRel))
    }
    fn take(cell: &AtomicUsize) -> Option<ExecutionPermit> {
        ExecutionPermit::take(cell.swap(0, Ordering::AcqRel))
    }
    fn initialize(&self, pin: ExecutionPermit) {
        assert_eq!(self.current.load(Ordering::Acquire), 0);
        assert_eq!(self.prepared.load(Ordering::Acquire), 0);
        assert_eq!(self.outgoing.load(Ordering::Acquire), 0);
        assert!(Self::put(&self.current, pin).is_none());
    }
    fn publish(&self, from: usize, to: usize) {
        let incoming = Self::take(&self.prepared);
        #[cfg(test)]
        if incoming.is_none() && self.current.load(Ordering::Acquire) == 0 {
            return;
        }
        assert_eq!(
            self.current.load(Ordering::Acquire),
            from + 1,
            "CPU execution lost original Current permit"
        );
        assert_eq!(
            self.outgoing.load(Ordering::Acquire),
            0,
            "execution transition overlaps prior RSP"
        );
        if from == to && incoming.is_none() {
            return;
        }
        let incoming = incoming.expect("CPU return lacks captured execution admission");
        assert_eq!(
            incoming.slot(),
            to,
            "CPU return spends foreign execution permit"
        );
        if from != to {
            let outgoing = Self::take(&self.current).unwrap();
            assert!(Self::put(&self.outgoing, outgoing).is_none());
        }
        drop(Self::put(&self.current, incoming));
    }
    fn commit(&self) {
        drop(Self::take(&self.outgoing));
    }
}
static BANKS: [Banks; nucleus_core::util::lockdep::MAX_TRACKED_CPUS] =
    [const { Banks::new() }; nucleus_core::util::lockdep::MAX_TRACKED_CPUS];
pub(in crate::multitask) fn initialize(cpu: usize, pin: ExecutionPermit) {
    BANKS[cpu].initialize(pin);
}
pub(in crate::multitask) fn prepare(cpu: usize, pin: ExecutionPermit) {
    #[cfg(rustos_boot_image)]
    assert_eq!(
        crate::multitask::cpu_local::scheduler_turn_logical_index(),
        cpu
    );
    drop(Banks::put(&BANKS[cpu].prepared, pin));
}
pub(in crate::multitask) fn publish(cpu: usize, from: usize, to: usize) {
    BANKS[cpu].publish(from, to);
}
pub(in crate::multitask) fn commit(cpu: usize) {
    BANKS[cpu].commit();
}

pub(in crate::multitask) fn owns_execution(cpu: usize, slot: usize) -> bool {
    BANKS.get(cpu).is_some_and(|bank| {
        bank.current.load(Ordering::Acquire) == slot + 1
            || bank.prepared.load(Ordering::Acquire) == slot + 1
    })
}

/// Non-Copy borrow of the ORIGINAL parked Current execution permit. The
/// same original masked scope retains it; no count increment or new admission.
pub(in crate::multitask) struct CurrentPermit<'scope, 'irq> {
    cell: &'static AtomicUsize,
    scope: &'scope nucleus_core::util::lockdep::DispatchPreemption<'irq>,
    slot: usize,
    affinity: core::marker::PhantomData<*mut ()>,
}
impl<'scope, 'irq> CurrentPermit<'scope, 'irq> {
    pub(in crate::multitask) fn borrow(
        scope: &'scope nucleus_core::util::lockdep::DispatchPreemption<'irq>, slot: usize,
    ) -> Option<Self> {
        if !scope.is_masked() || scope.cpu() != nucleus_core::util::lockdep::current_cpu_index() {
            return None;
        }
        let cell = &BANKS.get(scope.cpu())?.current;
        if cell.load(Ordering::Acquire) != slot.checked_add(1)? { return None; }
        Some(Self { cell, scope, slot, affinity: core::marker::PhantomData })
    }
    pub(in crate::multitask) fn check(&self) {
        assert!(self.scope.is_masked());
        assert_eq!(self.scope.cpu(), nucleus_core::util::lockdep::current_cpu_index());
        assert_eq!(self.cell.load(Ordering::Acquire), self.slot + 1,
            "original Current execution permit moved during borrow");
    }
}

/// Exact Current role, not merely a Prepared/Outgoing lifetime reference.
pub(in crate::multitask) fn owns_current(cpu: usize, slot: usize) -> bool {
    BANKS
        .get(cpu)
        .is_some_and(|bank| bank.current.load(Ordering::Acquire) == slot + 1)
}
pub(in crate::multitask) fn image_custody(slot: usize, current: Option<usize>) -> bool {
    match current {
        None => count(slot) == 0,
        Some(cpu) => count(slot) == 1 && BANKS[cpu].current.load(Ordering::Acquire) == slot + 1,
    }
}
#[cfg(test)]
pub(in crate::multitask) struct FixtureRestore {
    cpu: usize,
    prepared: Option<ExecutionPermit>,
    current: Option<ExecutionPermit>,
    outgoing: Option<ExecutionPermit>,
}
#[cfg(test)]
pub(in crate::multitask) fn isolate(cpu: usize) -> FixtureRestore {
    let cell = &BANKS[cpu];
    FixtureRestore {
        cpu,
        prepared: Banks::take(&cell.prepared),
        current: Banks::take(&cell.current),
        outgoing: Banks::take(&cell.outgoing),
    }
}
#[cfg(test)]
impl Drop for FixtureRestore {
    fn drop(&mut self) {
        let cell = &BANKS[self.cpu];
        for (bank, saved) in [
            (&cell.prepared, self.prepared.take()),
            (&cell.current, self.current.take()),
            (&cell.outgoing, self.outgoing.take()),
        ] {
            drop(Banks::take(bank));
            if let Some(saved) = saved {
                assert!(Banks::put(bank, saved).is_none());
            }
        }
    }
}
#[cfg(test)]
mod tests;
