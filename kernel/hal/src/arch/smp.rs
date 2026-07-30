//! Generation-bound CPU lifecycle publication and transition authority.
//!
//! - **Owner:** `kernel-hal` owns logical CPU identity and architectural
//!   online-state transitions; scheduling policy remains in `kernel-ps`.
//! - **Boundary:** Only a completely admitted ACPI topology may create slots.
//!   Raw APIC IDs are data and never array indexes.
//! - **Lifecycle:** One release-published registry moves each exact generation
//!   through Discovered -> Starting -> OnlineParked -> SchedulerReady -> Online
//!   or the documented quarantine/failure paths.
//! - **Concurrency:** Boot publication is single-writer; readers acquire the
//!   publication epoch and state transitions use atomic compare-exchange.
//! - **Failure:** Missing untrusted topology leaves the registry unpublished.
//!   Duplicate publication, stale generation, and illegal internal transition
//!   panic because continuing could duplicate CPU or task authority.
//! - **Forbidden:** No skipped state, generation zero/wrap, raw APIC indexing,
//!   or relaxed publication of initialized CPU-local authority.
//! - **Evidence:** `cpu-topology-admission` and `cpu-online-lifecycle`.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};

use super::acpi::{CpuDescriptor, CpuTopology, MAX_SUPPORTED_CPUS};

const REGISTRY_EMPTY: u8 = 0;
const REGISTRY_BUILDING: u8 = 1;
const REGISTRY_PUBLISHED: u8 = 2;
const FIRST_CPU_GENERATION: u64 = 1;
const AP_BOOTSTRAP_STACK_SIZE: usize = 64 * 1024;

#[repr(align(16))]
struct ApBootstrapStack {
    _bytes: [u8; AP_BOOTSTRAP_STACK_SIZE],
}

struct ApBootstrapStackMemory(UnsafeCell<ApBootstrapStack>);

// SAFETY: every stack has one permanent logical CPU owner and is published to
// exactly one AP startup mailbox while that CPU is in Starting.
unsafe impl Sync for ApBootstrapStackMemory {}

static AP_BOOTSTRAP_STACKS: [ApBootstrapStackMemory; MAX_SUPPORTED_CPUS] = [const {
    ApBootstrapStackMemory(UnsafeCell::new(ApBootstrapStack {
        _bytes: [0; AP_BOOTSTRAP_STACK_SIZE],
    }))
}; MAX_SUPPORTED_CPUS];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum CpuLifecycleState {
    Discovered = 1,
    Starting = 2,
    OnlineParked = 3,
    SchedulerReady = 4,
    Online = 5,
    Quarantined = 6,
    Failed = 7,
}

impl CpuLifecycleState {
    fn from_raw(raw: u8) -> Self {
        match raw {
            1 => Self::Discovered,
            2 => Self::Starting,
            3 => Self::OnlineParked,
            4 => Self::SchedulerReady,
            5 => Self::Online,
            6 => Self::Quarantined,
            7 => Self::Failed,
            _ => panic!("SMP invariant: invalid CPU lifecycle state {raw}"),
        }
    }

    const fn may_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Discovered, Self::Starting)
                | (Self::Starting, Self::OnlineParked)
                | (Self::Starting, Self::Failed)
                | (Self::OnlineParked, Self::SchedulerReady)
                | (Self::OnlineParked, Self::Failed)
                | (Self::SchedulerReady, Self::Online)
                | (Self::SchedulerReady, Self::Failed)
                | (Self::Online, Self::Quarantined)
                | (Self::Quarantined, Self::Failed)
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CpuLifecycleSnapshot {
    pub logical_index: u8,
    pub firmware_uid: u32,
    pub apic_id: u32,
    pub uses_x2apic_id: bool,
    pub generation: u64,
    pub state: CpuLifecycleState,
}

struct CpuLifecycleSlot {
    firmware_uid: AtomicU32,
    apic_id: AtomicU32,
    uses_x2apic_id: AtomicBool,
    generation: AtomicU64,
    state: AtomicU8,
}

impl CpuLifecycleSlot {
    const fn empty() -> Self {
        Self {
            firmware_uid: AtomicU32::new(u32::MAX),
            apic_id: AtomicU32::new(u32::MAX),
            uses_x2apic_id: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            state: AtomicU8::new(0),
        }
    }

    fn initialize(&self, descriptor: CpuDescriptor) {
        // ORDERING: the registry's final Release publication makes all of
        // these boot-serialized Relaxed field stores visible to Acquire readers.
        self.firmware_uid
            .store(descriptor.firmware_uid, Ordering::Relaxed);
        self.apic_id.store(descriptor.apic_id, Ordering::Relaxed);
        self.uses_x2apic_id
            .store(descriptor.uses_x2apic_id, Ordering::Relaxed);
        self.generation
            .store(FIRST_CPU_GENERATION, Ordering::Relaxed);
        self.state
            .store(CpuLifecycleState::Discovered as u8, Ordering::Relaxed);
    }
}

pub struct CpuLifecycleRegistry {
    publication: AtomicU8,
    cpu_count: AtomicU8,
    slots: [CpuLifecycleSlot; MAX_SUPPORTED_CPUS],
}

impl CpuLifecycleRegistry {
    pub const fn new() -> Self {
        Self {
            publication: AtomicU8::new(REGISTRY_EMPTY),
            cpu_count: AtomicU8::new(0),
            slots: [const { CpuLifecycleSlot::empty() }; MAX_SUPPORTED_CPUS],
        }
    }

    pub fn publish_discovered(&self, topology: CpuTopology) {
        let count = topology.cpu_count();
        assert!(
            (1..=MAX_SUPPORTED_CPUS).contains(&count),
            "SMP invariant: published topology has invalid CPU count {count}"
        );
        for (index, descriptor) in topology.cpus().iter().copied().enumerate() {
            assert_eq!(
                usize::from(descriptor.logical_index),
                index,
                "SMP invariant: non-dense logical CPU index"
            );
            assert!(
                topology.cpus()[..index].iter().all(|prior| {
                    prior.firmware_uid != descriptor.firmware_uid
                        && prior.apic_id != descriptor.apic_id
                }),
                "SMP invariant: duplicate CPU identity crossed ACPI admission"
            );
        }

        // ORDERING: AcqRel claims the sole boot publication epoch. A competing
        // builder/publisher is an internal ownership violation.
        if self
            .publication
            .compare_exchange(
                REGISTRY_EMPTY,
                REGISTRY_BUILDING,
                // ORDERING: AcqRel claims the unique publication epoch.
                Ordering::AcqRel,
                // ORDERING: Acquire observes a competing publisher's state.
                Ordering::Acquire,
            )
            .is_err()
        {
            panic!("SMP invariant: CPU lifecycle registry published more than once");
        }
        for descriptor in topology.cpus().iter().copied() {
            self.slots[usize::from(descriptor.logical_index)].initialize(descriptor);
        }
        self.cpu_count.store(count as u8, Ordering::Relaxed);
        // ORDERING: Release publishes every initialized descriptor, generation,
        // state, and count to readers that first Acquire `publication`.
        self.publication
            .store(REGISTRY_PUBLISHED, Ordering::Release);
    }

    pub fn cpu_count(&self) -> usize {
        // ORDERING: Acquire pairs with final topology publication before the
        // Relaxed count/descriptor fields can be observed.
        if self.publication.load(Ordering::Acquire) != REGISTRY_PUBLISHED {
            return 0;
        }
        usize::from(self.cpu_count.load(Ordering::Relaxed))
    }

    pub fn snapshot(&self, logical_index: u8) -> Option<CpuLifecycleSnapshot> {
        let index = usize::from(logical_index);
        if index >= self.cpu_count() {
            return None;
        }
        let slot = &self.slots[index];
        // ORDERING: Acquire observes the latest lifecycle transition before
        // consumers act on the state and its generation-bound authority.
        let state = CpuLifecycleState::from_raw(slot.state.load(Ordering::Acquire));
        Some(CpuLifecycleSnapshot {
            logical_index,
            firmware_uid: slot.firmware_uid.load(Ordering::Relaxed),
            apic_id: slot.apic_id.load(Ordering::Relaxed),
            uses_x2apic_id: slot.uses_x2apic_id.load(Ordering::Relaxed),
            generation: slot.generation.load(Ordering::Relaxed),
            state,
        })
    }

    fn online_mask(&self) -> u64 {
        let count = self.cpu_count();
        let mut mask = 0_u64;
        for logical_index in 0..count {
            let logical_index =
                u8::try_from(logical_index).expect("SMP invariant: logical CPU index overflow");
            let snapshot = self
                .snapshot(logical_index)
                .expect("SMP invariant: published topology lost a CPU lifecycle slot");
            if snapshot.state == CpuLifecycleState::Online {
                mask |= 1_u64 << logical_index;
            }
        }
        mask
    }

    pub fn transition(&self, logical_index: u8, expected_generation: u64, next: CpuLifecycleState) {
        let Some(snapshot) = self.snapshot(logical_index) else {
            panic!("SMP invariant: transition targets absent CPU {logical_index}");
        };
        assert_eq!(
            snapshot.generation, expected_generation,
            "SMP invariant: stale CPU generation for logical CPU {logical_index}"
        );
        assert!(
            snapshot.state.may_transition_to(next),
            "SMP invariant: illegal CPU transition {:?} -> {:?} for logical CPU {} generation {}",
            snapshot.state,
            next,
            logical_index,
            expected_generation,
        );
        let slot = &self.slots[usize::from(logical_index)];
        // ORDERING: AcqRel linearizes the exact generation's ownership
        // transition; failure means another CPU changed the state concurrently.
        if slot
            .state
            .compare_exchange(
                snapshot.state as u8,
                next as u8,
                // ORDERING: AcqRel publishes the exact state ownership transfer.
                Ordering::AcqRel,
                // ORDERING: Acquire observes the winner of a raced transition.
                Ordering::Acquire,
            )
            .is_err()
        {
            panic!(
                "SMP invariant: raced CPU transition for logical CPU {logical_index} generation {expected_generation}"
            );
        }
    }
}

static CPU_LIFECYCLE: CpuLifecycleRegistry = CpuLifecycleRegistry::new();

pub fn stage_discovered_topology() {
    if let Some(topology) = super::acpi::cpu_topology() {
        for cpu in topology.cpus().iter().copied() {
            nucleus_core::util::lockdep::register_cpu_identity(cpu.logical_index, cpu.apic_id);
        }
        nucleus_core::util::lockdep::finalize_cpu_identities(topology.cpu_count());
        CPU_LIFECYCLE.publish_discovered(topology);
        let bsp = CPU_LIFECYCLE
            .snapshot(0)
            .expect("SMP invariant: admitted topology omitted logical BSP");
        assert_eq!(
            bsp.apic_id,
            nucleus_core::util::lockdep::hardware_apic_id(),
            "SMP invariant: logical CPU zero is not the executing BSP"
        );
        CPU_LIFECYCLE.transition(0, bsp.generation, CpuLifecycleState::Starting);
    }
}

pub fn cpu_count() -> usize {
    CPU_LIFECYCLE.cpu_count()
}

/// Returns the exact dense commercial Online set.
///
/// Partial topology is forbidden after scheduler admission: exposing a smaller
/// affinity mask would let a failed AP masquerade as a supported topology.
pub fn admitted_online_mask() -> u64 {
    let count = CPU_LIFECYCLE.cpu_count();
    assert!(
        (1..=MAX_SUPPORTED_CPUS).contains(&count),
        "SMP invariant: affinity observation before topology publication"
    );
    let expected = (1_u64 << count) - 1;
    let online = CPU_LIFECYCLE.online_mask();
    assert_eq!(
        online, expected,
        "SMP invariant: partial Online topology at affinity observation"
    );
    online
}

pub fn snapshot(logical_index: u8) -> Option<CpuLifecycleSnapshot> {
    CPU_LIFECYCLE.snapshot(logical_index)
}

pub fn transition(logical_index: u8, expected_generation: u64, next: CpuLifecycleState) {
    CPU_LIFECYCLE.transition(logical_index, expected_generation, next);
}

pub fn ap_bootstrap_stack_top(logical_index: u8, expected_generation: u64) -> u64 {
    let snapshot = CPU_LIFECYCLE
        .snapshot(logical_index)
        .unwrap_or_else(|| panic!("SMP invariant: AP stack targets absent CPU {logical_index}"));
    assert_ne!(
        logical_index, 0,
        "SMP invariant: BSP cannot consume an AP bootstrap stack"
    );
    assert_eq!(
        snapshot.generation, expected_generation,
        "SMP invariant: AP stack request used a stale generation"
    );
    assert_eq!(
        snapshot.state,
        CpuLifecycleState::Starting,
        "SMP invariant: AP stack published outside Starting"
    );
    let stack = AP_BOOTSTRAP_STACKS[usize::from(logical_index)].0.get();
    stack as u64 + AP_BOOTSTRAP_STACK_SIZE as u64
}

pub fn ap_bootstrap_stack_bounds(logical_index: u8, expected_generation: u64) -> (u64, u64) {
    let snapshot = CPU_LIFECYCLE
        .snapshot(logical_index)
        .unwrap_or_else(|| panic!("SMP invariant: idle stack targets absent CPU {logical_index}"));
    assert_ne!(
        logical_index, 0,
        "SMP invariant: BSP cannot consume an AP idle stack"
    );
    assert_eq!(
        snapshot.generation, expected_generation,
        "SMP invariant: idle stack request used a stale generation"
    );
    // The same physical stack is deliberately handed from the serial
    // trampoline owner to the scheduler only after the AP has acknowledged
    // OnlineParked. Requiring Starting here would make that valid handoff
    // impossible; accepting SchedulerReady or Online would permit reuse after
    // dispatch and violate the single-owner stack contract.
    assert_eq!(
        snapshot.state,
        CpuLifecycleState::OnlineParked,
        "SMP invariant: AP idle stack consumed outside OnlineParked"
    );
    let stack = AP_BOOTSTRAP_STACKS[usize::from(logical_index)].0.get();
    let top = stack as u64 + AP_BOOTSTRAP_STACK_SIZE as u64;
    (
        top.checked_sub(AP_BOOTSTRAP_STACK_SIZE as u64)
            .expect("SMP invariant: AP bootstrap stack range underflow"),
        top,
    )
}

#[cfg(test)]
mod tests {
    use super::{
        AP_BOOTSTRAP_STACK_SIZE, AP_BOOTSTRAP_STACKS, CpuLifecycleRegistry, CpuLifecycleState,
        FIRST_CPU_GENERATION, MAX_SUPPORTED_CPUS,
    };
    use crate::arch::acpi::test_topology;

    #[test]
    fn cpu_lifecycle_publication_is_dense_generation_bound_and_ordered() {
        let registry = CpuLifecycleRegistry::new();
        registry.publish_discovered(test_topology(&[(7, 3, false), (42, 0x1234, true)]));
        assert_eq!(registry.cpu_count(), 2);

        let first = registry.snapshot(0).expect("first CPU");
        assert_eq!(first.generation, FIRST_CPU_GENERATION);
        assert_eq!(first.state, CpuLifecycleState::Discovered);
        registry.transition(0, first.generation, CpuLifecycleState::Starting);
        registry.transition(0, first.generation, CpuLifecycleState::OnlineParked);
        registry.transition(0, first.generation, CpuLifecycleState::SchedulerReady);
        registry.transition(0, first.generation, CpuLifecycleState::Online);
        assert_eq!(
            registry.snapshot(0).map(|cpu| cpu.state),
            Some(CpuLifecycleState::Online)
        );
    }

    #[test]
    fn online_mask_contains_exact_dense_online_set() {
        let registry = CpuLifecycleRegistry::new();
        registry.publish_discovered(test_topology(&[(7, 3, false), (42, 0x1234, true)]));
        for logical_index in 0..2 {
            registry.transition(
                logical_index,
                FIRST_CPU_GENERATION,
                CpuLifecycleState::Starting,
            );
            registry.transition(
                logical_index,
                FIRST_CPU_GENERATION,
                CpuLifecycleState::OnlineParked,
            );
            registry.transition(
                logical_index,
                FIRST_CPU_GENERATION,
                CpuLifecycleState::SchedulerReady,
            );
        }
        registry.transition(0, FIRST_CPU_GENERATION, CpuLifecycleState::Online);
        assert_eq!(registry.online_mask(), 0b01);
        registry.transition(1, FIRST_CPU_GENERATION, CpuLifecycleState::Online);
        assert_eq!(registry.online_mask(), 0b11);
    }

    #[test]
    #[should_panic(expected = "illegal CPU transition")]
    fn cpu_lifecycle_rejects_skipped_state() {
        let registry = CpuLifecycleRegistry::new();
        registry.publish_discovered(test_topology(&[(0, 0, false)]));
        registry.transition(0, FIRST_CPU_GENERATION, CpuLifecycleState::Online);
    }

    #[test]
    #[should_panic(expected = "stale CPU generation")]
    fn cpu_lifecycle_rejects_stale_generation() {
        let registry = CpuLifecycleRegistry::new();
        registry.publish_discovered(test_topology(&[(0, 0, false)]));
        registry.transition(0, FIRST_CPU_GENERATION + 1, CpuLifecycleState::Starting);
    }

    #[test]
    fn ap_bootstrap_stacks_are_aligned_and_disjoint() {
        let mut bases = [0_usize; MAX_SUPPORTED_CPUS];
        for cpu in 0..MAX_SUPPORTED_CPUS {
            bases[cpu] = AP_BOOTSTRAP_STACKS[cpu].0.get() as usize;
            assert_eq!(bases[cpu] & 0xf, 0);
            assert!(!bases[..cpu].contains(&bases[cpu]));
            for prior in &bases[..cpu] {
                assert!(
                    bases[cpu] >= prior.saturating_add(AP_BOOTSTRAP_STACK_SIZE)
                        || *prior >= bases[cpu].saturating_add(AP_BOOTSTRAP_STACK_SIZE)
                );
            }
        }
    }
}
