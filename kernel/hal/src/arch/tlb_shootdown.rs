//! Generation-bound x86_64 TLB shootdown and address-space activation.
//!
//! - **Owner:** `kernel-hal` owns CR3 activation, the active-CPU registry, the
//!   fixed IPI mailbox, and acknowledgement deadline.
//! - **Boundary:** `kernel-mm` must hold an `AddressSpaceMutationGuard` across
//!   every page-table edit and frame-reclaim decision.
//! - **Lifecycle:** serialize mutations → publish exact generation mailboxes
//!   to every admitted CPU → flush local/remote translations → acknowledge →
//!   permit reuse. Address-space activation publishes CR3/root lock-free.
//! - **Concurrency:** the protocol lock is mutation-sender-only and is never
//!   acquired by IRQ-time address-space activation or the IPI leaf. An
//!   address-space mutation targets the CPUs whose release-published active
//!   root matches; global kernel mappings still target every eligible CPU.
//! - **Failure:** missing eligibility, root mismatch, generation wrap, stale
//!   mailbox, or acknowledgement timeout is an immediate kernel panic.
//! - **Forbidden:** no CR3 write outside this module after SMP admission, no
//!   frame reclaim before guard completion, and no handler-side lock.
//! - **Evidence:** `tlb-shootdown-lifecycle`.
//!
//! # Unacknowledged-target policy
//!
//! The supported envelope has no CPU hot-remove, so a target that does not
//! acknowledge within `ACK_LIVENESS_TIMEOUT_NS` cannot be retired and its
//! outstanding translation cannot be proven dead. The chosen policy is
//! therefore **system fail-stop**, not CPU quarantine: the watchdog panics
//! while the frame is still held behind its mutation guard.
//!
//! The rejected alternative is to let the deadline expire into reclaim. That
//! would free a frame a live CPU may still translate, which is memory
//! corruption rather than a latency problem, so no timeout on this path may
//! ever authorize reuse. A future quarantine policy would first have to add
//! `Draining -> Parked -> Offline` CPU states, prove the quarantined CPU owns
//! no runnable task and no reclaim-blocking acknowledgement, and only then may
//! weaken this fail-stop.
//!
//! The deadline is a dead-owner horizon, not a latency budget: a virtualized
//! target can be descheduled by its host for a long time while remaining
//! perfectly healthy, so the bound is deliberately generous and the retry IPI
//! carries liveness in the meantime.

#[cfg(rustos_boot_image)]
use core::hint::spin_loop;
#[cfg(rustos_boot_image)]
use core::sync::atomic::AtomicBool;
use core::sync::atomic::{AtomicU64, Ordering};

use nucleus_core::util::lockdep::{LockClass, TrackedSpinGuard, TrackedSpinLock};
use x86_64::PhysAddr;
#[cfg(rustos_boot_image)]
use x86_64::instructions::interrupts;
use x86_64::instructions::tlb;
#[cfg(rustos_boot_image)]
use x86_64::registers::control::{Cr3, Cr3Flags};
#[cfg(rustos_boot_image)]
use x86_64::structures::paging::PhysFrame;

use super::acpi::MAX_SUPPORTED_CPUS;

const PAGE_SIZE: u64 = 4096;
const GLOBAL_SCOPE: u64 = 0;
#[cfg(rustos_boot_image)]
/// A delayed virtual CPU may legitimately miss one host scheduling quantum.
/// Retry notification at this bound; it is not a CPU-failure verdict.
const ACK_RETRY_NS: u64 = 100_000_000;
/// The boot-static SMP envelope is fail-stop. Only this longer liveness
/// watchdog may classify a target as unable to participate in safe reclaim.
#[cfg(rustos_boot_image)]
const ACK_LIVENESS_TIMEOUT_NS: u64 = 2_000_000_000;
#[cfg(rustos_boot_image)]
const PROTOCOL_ACQUIRE_TIMEOUT_NS: u64 = ACK_LIVENESS_TIMEOUT_NS;

static PROTOCOL_LOCK: TrackedSpinLock<(), { LockClass::TlbShootdown as u8 }> =
    TrackedSpinLock::new(());
#[cfg(rustos_boot_image)]
static ACTIVE_ROOT: [AtomicU64; MAX_SUPPORTED_CPUS] =
    [const { AtomicU64::new(0) }; MAX_SUPPORTED_CPUS];
#[cfg(rustos_boot_image)]
static SHOOTDOWN_ELIGIBLE: [AtomicBool; MAX_SUPPORTED_CPUS] =
    [const { AtomicBool::new(false) }; MAX_SUPPORTED_CPUS];
#[cfg(rustos_boot_image)]
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(0);
static REQUEST_ROOT: [AtomicU64; MAX_SUPPORTED_CPUS] =
    [const { AtomicU64::new(0) }; MAX_SUPPORTED_CPUS];
static REQUEST_GENERATION: [AtomicU64; MAX_SUPPORTED_CPUS] =
    [const { AtomicU64::new(0) }; MAX_SUPPORTED_CPUS];
static ACK_GENERATION: [AtomicU64; MAX_SUPPORTED_CPUS] =
    [const { AtomicU64::new(0) }; MAX_SUPPORTED_CPUS];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MutationScope {
    AddressSpace(u64),
    Global,
}

pub struct AddressSpaceMutationGuard {
    scope: MutationScope,
    guard: Option<TrackedSpinGuard<'static, (), { LockClass::TlbShootdown as u8 }>>,
    restore_interrupts: bool,
}

/// A mutation whose exact shootdown quorum has completed.  Holding this value
/// retains serialization while the caller removes ownership metadata and
/// releases frames, but dropping it only unlocks the protocol; it cannot issue
/// a second shootdown for the same mutation generation.
pub struct FlushedAddressSpaceMutationGuard {
    guard: Option<TrackedSpinGuard<'static, (), { LockClass::TlbShootdown as u8 }>>,
    restore_interrupts: bool,
}

impl AddressSpaceMutationGuard {
    /// Complete a shootdown while retaining mutation serialization.
    ///
    /// Call this after removing a mapping and before releasing or reassigning
    /// any frame that a stale translation could still reach.
    pub fn flush_for_reclaim(mut self) -> FlushedAddressSpaceMutationGuard {
        assert!(
            self.guard.is_some(),
            "TLB invariant: inactive mutation guard attempted a flush"
        );
        #[cfg(rustos_boot_image)]
        finish_shootdown(self.scope);
        let guard = self
            .guard
            .take()
            .expect("TLB invariant: flushed mutation guard lost its protocol lock");
        let flushed = FlushedAddressSpaceMutationGuard {
            guard: Some(guard),
            restore_interrupts: self.restore_interrupts,
        };
        // `AddressSpaceMutationGuard` has a Drop implementation for the dirty
        // state. Ownership of its protocol guard has moved into the flushed
        // typestate, so suppress that dirty-state destructor exactly once.
        core::mem::forget(self);
        flushed
    }
}

impl Drop for FlushedAddressSpaceMutationGuard {
    fn drop(&mut self) {
        let guard = self
            .guard
            .take()
            .expect("TLB invariant: flushed mutation guard completed twice");
        unlock_protocol(guard, self.restore_interrupts);
    }
}

pub fn begin_address_space_mutation(root: PhysAddr) -> AddressSpaceMutationGuard {
    let root = root.as_u64();
    assert!(
        root != 0 && root.is_multiple_of(PAGE_SIZE),
        "TLB invariant: address-space mutation root is invalid"
    );
    let (guard, restore_interrupts) = lock_protocol_bounded();
    AddressSpaceMutationGuard {
        scope: MutationScope::AddressSpace(root),
        guard: Some(guard),
        restore_interrupts,
    }
}

pub fn begin_global_mapping_mutation() -> AddressSpaceMutationGuard {
    let (guard, restore_interrupts) = lock_protocol_bounded();
    AddressSpaceMutationGuard {
        scope: MutationScope::Global,
        guard: Some(guard),
        restore_interrupts,
    }
}

/// Hold the protocol lock from the final inactive-root check through the
/// global shootdown completed by the returned guard.
pub fn begin_address_space_retirement(root: PhysAddr) -> AddressSpaceMutationGuard {
    let root_raw = root.as_u64();
    assert!(
        root_raw != 0 && root_raw.is_multiple_of(PAGE_SIZE),
        "TLB invariant: retired address-space root is invalid"
    );
    let (guard, restore_interrupts) = lock_protocol_bounded();
    #[cfg(rustos_boot_image)]
    assert_address_space_inactive_locked(root_raw);
    AddressSpaceMutationGuard {
        scope: MutationScope::Global,
        guard: Some(guard),
        restore_interrupts,
    }
}

impl Drop for AddressSpaceMutationGuard {
    fn drop(&mut self) {
        assert!(
            self.guard.is_some(),
            "TLB invariant: mutation guard completed twice"
        );
        #[cfg(rustos_boot_image)]
        finish_shootdown(self.scope);
        #[cfg(not(rustos_boot_image))]
        let _ = (self.scope, self.restore_interrupts);
        let guard = self
            .guard
            .take()
            .expect("TLB invariant: mutation guard lost its protocol lock");
        unlock_protocol(guard, self.restore_interrupts);
    }
}

fn lock_protocol_bounded() -> (
    TrackedSpinGuard<'static, (), { LockClass::TlbShootdown as u8 }>,
    bool,
) {
    #[cfg(rustos_boot_image)]
    {
        assert_eq!(
            nucleus_core::util::lockdep::irq_context_depth(),
            0,
            "TLB invariant: mutation protocol entered from IRQ context"
        );
        let restore_interrupts = interrupts::are_enabled();
        let mut deadline = super::clock::SpinDeadline::start();
        loop {
            if restore_interrupts {
                interrupts::disable();
            }
            if let Some(guard) = PROTOCOL_LOCK.try_lock() {
                return (guard, restore_interrupts);
            }
            if restore_interrupts {
                // A remote sender holding the protocol lock may require this
                // CPU's shootdown acknowledgement. Re-enable between attempts
                // so lock contention cannot suppress the very IPI that lets
                // the current owner finish.
                interrupts::enable();
            }
            // Cross-CPU mapping mutations and an AP's first shootdown admission
            // may contend legitimately. Use the protocol's wall-clock bound,
            // not the generic diagnostic spin count, while still failing
            // closed on a lost owner or circular dependency.
            if deadline.elapsed_nanos() >= PROTOCOL_ACQUIRE_TIMEOUT_NS {
                panic!("TLB invariant: protocol lock acquisition timed out");
            }
            spin_loop();
        }
    }
    #[cfg(not(rustos_boot_image))]
    {
        (PROTOCOL_LOCK.lock(), false)
    }
}

fn unlock_protocol(
    guard: TrackedSpinGuard<'static, (), { LockClass::TlbShootdown as u8 }>,
    restore_interrupts: bool,
) {
    drop(guard);
    #[cfg(rustos_boot_image)]
    if restore_interrupts {
        // The matching bounded acquisition disabled local interrupts only
        // after recording that they were enabled on entry.
        interrupts::enable();
    }
    #[cfg(not(rustos_boot_image))]
    let _ = restore_interrupts;
}

/// Load one address-space root and publish it as this CPU's active root.
///
/// This path runs in timer/software-schedule IRQ context and therefore must
/// never wait for the mutation sender lock. A changed activation writes CR3
/// before release-publishing the new root. Therefore a sender either observes
/// the old matching root and targets it, or observes a different root whose
/// preceding CR3 write already discarded translations; any later activation
/// back to the mutation root performs another CR3 write before publication.
pub fn activate_address_space(root: PhysAddr) {
    let root_raw = root.as_u64();
    assert!(
        root_raw != 0 && root_raw.is_multiple_of(PAGE_SIZE),
        "TLB invariant: active CR3 root is invalid"
    );
    #[cfg(rustos_boot_image)]
    interrupts::without_interrupts(|| {
        let logical_index = current_cpu_index();
        // ORDERING: Acquire observes this CPU's previous Release publication.
        // A sender filters address-space mutations by this publication. A
        // changed root is written to CR3 before the Release below, while an
        // unchanged root remains an exact target for a concurrent mutation.
        // ORDERING: This Acquire is the exact same-CPU publication check.
        let active_root = ACTIVE_ROOT[logical_index].load(Ordering::Acquire);
        if !activation_requires_cr3_write(active_root, root_raw) {
            return;
        }
        write_cr3(root);
        // ORDERING: Release publishes the active root only after CR3 accepted
        // the same frame. A sender's Acquire may use this value for exact-root
        // target selection only because every changed activation flushes via
        // CR3 before this publication.
        ACTIVE_ROOT[logical_index].store(root_raw, Ordering::Release);
    });
}

/// Complete the pre-dispatch admission barrier for the current CPU.
///
/// An AP is not a shootdown target before this call. Reloading CR3 immediately
/// before eligibility closes the parked-to-online window without requiring a
/// generation token in the no-payload reschedule protocol.
pub fn admit_current_cpu_online() {
    #[cfg(rustos_boot_image)]
    {
        let (guard, restore_interrupts) = lock_protocol_bounded();
        let logical_index = current_cpu_index();
        // ORDERING: Acquire observes the root published by activation.
        let root = ACTIVE_ROOT[logical_index].load(Ordering::Acquire);
        assert_ne!(
            root, 0,
            "TLB invariant: CPU admitted online without an active root"
        );
        write_cr3(PhysAddr::new(root));
        // ORDERING: Release publishes eligibility only after the admission
        // reload discarded every translation acquired while parked.
        assert!(
            SHOOTDOWN_ELIGIBLE[logical_index]
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
            "TLB invariant: CPU admitted to shootdowns twice"
        );
        unlock_protocol(guard, restore_interrupts);
    }
}

/// Fail closed before kernel-mm releases an address-space page-table tree.
///
/// Task/process retirement is responsible for preventing a future activation.
/// This final HAL check proves that every CPU which can receive a shootdown has
/// already stopped using the root. A local-only CR3 comparison is insufficient
/// once another CPU can run the same process.
pub fn assert_address_space_inactive(root: PhysAddr) {
    let root_raw = root.as_u64();
    assert!(
        root_raw != 0 && root_raw.is_multiple_of(PAGE_SIZE),
        "TLB invariant: reclaimed address-space root is invalid"
    );
    #[cfg(rustos_boot_image)]
    {
        let (guard, restore_interrupts) = lock_protocol_bounded();
        assert_address_space_inactive_locked(root_raw);
        unlock_protocol(guard, restore_interrupts);
    }
}

#[cfg(rustos_boot_image)]
fn assert_address_space_inactive_locked(root_raw: u64) {
    for logical_index in 0..MAX_SUPPORTED_CPUS {
        // ORDERING: Acquire pairs with activation publication and online
        // admission while the protocol lock excludes a concurrent change.
        if SHOOTDOWN_ELIGIBLE[logical_index].load(Ordering::Acquire)
            && ACTIVE_ROOT[logical_index].load(Ordering::Acquire) == root_raw
        {
            panic!(
                "TLB invariant: address-space root {root_raw:#x} reclaimed while active on logical CPU {logical_index}"
            );
        }
    }
}

#[cfg(rustos_boot_image)]
fn finish_shootdown(scope: MutationScope) {
    // ORDERING: AcqRel allocates one globally serialized non-aliasing mailbox
    // generation and observes any prior completed protocol.
    let generation = NEXT_GENERATION
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            current.checked_add(1)
        })
        .unwrap_or_else(|_| panic!("TLB invariant: shootdown generation exhausted"))
        .checked_add(1)
        .expect("TLB invariant: shootdown generation wrapped");
    let current = current_cpu_index();
    let mut targets = [false; MAX_SUPPORTED_CPUS];
    let Some(topology) = super::acpi::cpu_topology() else {
        tlb::flush_all();
        return;
    };

    let mutation_root = scope_root(scope);
    for descriptor in topology.cpus() {
        let target = usize::from(descriptor.logical_index);
        // ORDERING: Acquire observes online admission before target selection.
        let eligible = SHOOTDOWN_ELIGIBLE[target].load(Ordering::Acquire);
        // ORDERING: Acquire observes the root published after the target's CR3
        // write. If it differs from an address-space mutation root, the prior
        // write already discarded old translations; activation back to the
        // mutated root must perform and publish another CR3 write.
        let active_root = ACTIVE_ROOT[target].load(Ordering::Acquire);
        if target == current || !shootdown_target_is_required(eligible, active_root, mutation_root)
        {
            continue;
        }
        assert_ne!(
            active_root, 0,
            "TLB invariant: eligible target has no active root"
        );
        // ORDERING: These Relaxed payload fields are not authority on their
        // own; the following Release request generation publishes both.
        ACK_GENERATION[target].store(0, Ordering::Relaxed);
        REQUEST_ROOT[target].store(mutation_root, Ordering::Relaxed);
        // ORDERING: Release publishes the complete target mailbox.
        REQUEST_GENERATION[target].store(generation, Ordering::Release);
        targets[target] = true;
    }

    // The sender flushes unconditionally because it owns the page-table edit
    // even when its current root changed during the mutation transaction.
    tlb::flush_all();

    for descriptor in topology.cpus() {
        let target = usize::from(descriptor.logical_index);
        if targets[target] {
            super::msi::send_tlb_shootdown_ipi(descriptor.apic_id).unwrap_or_else(|error| {
                panic!(
                    "TLB invariant: generation {generation} IPI to logical CPU {} APIC {} failed: {error:?}",
                    descriptor.logical_index, descriptor.apic_id
                )
            });
        }
    }

    // One batched deadline spans the complete acknowledgement quorum, so the
    // liveness horizon still covers the whole shootdown rather than resetting
    // per target.
    let mut deadline = super::clock::SpinDeadline::start();
    for descriptor in topology.cpus() {
        let target = usize::from(descriptor.logical_index);
        if !targets[target] {
            continue;
        }
        let mut retry_after = ACK_RETRY_NS;
        loop {
            // ORDERING: Acquire observes the target flush preceding its exact
            // acknowledgement publication.
            if ACK_GENERATION[target].load(Ordering::Acquire) == generation {
                break;
            }
            let elapsed = deadline.elapsed_nanos();
            if elapsed >= ACK_LIVENESS_TIMEOUT_NS {
                panic!(
                    "TLB liveness watchdog: generation {generation} retained reclaim quarantine while logical CPU {} APIC {} failed to acknowledge after {}ns",
                    descriptor.logical_index,
                    descriptor.apic_id,
                    deadline.exact_elapsed_nanos()
                );
            }
            if elapsed >= retry_after {
                // The mailbox remains generation-owned and the frame remains
                // quarantined behind this guard. A duplicate IPI is only a
                // notification retry; the exact generation ACK is idempotent.
                super::msi::send_tlb_shootdown_ipi(descriptor.apic_id).unwrap_or_else(|error| {
                    panic!(
                        "TLB invariant: generation {generation} retry IPI to logical CPU {} APIC {} failed: {error:?}",
                        descriptor.logical_index, descriptor.apic_id
                    )
                });
                retry_after = elapsed.saturating_add(ACK_RETRY_NS);
            }
            spin_loop();
        }
    }
    assert!(
        acknowledgements_complete(&targets, &ACK_GENERATION, generation),
        "TLB invariant: reclaim reached without every exact acknowledgement"
    );
}

pub(super) fn on_interrupt() {
    let logical_index = current_cpu_index();
    // ORDERING: Acquire observes the complete mailbox before root validation.
    let generation = REQUEST_GENERATION[logical_index].load(Ordering::Acquire);
    assert_ne!(
        generation, 0,
        "TLB invariant: target received an unpublished shootdown"
    );
    let request_root = REQUEST_ROOT[logical_index].load(Ordering::Relaxed);
    assert!(
        request_root == GLOBAL_SCOPE || request_root.is_multiple_of(PAGE_SIZE),
        "TLB invariant: runtime shootdown scope is invalid"
    );
    tlb::flush_all();
    // ORDERING: Release publishes flush completion for the exact generation.
    ACK_GENERATION[logical_index].store(generation, Ordering::Release);
    super::msi::local_apic_eoi();
}

#[cfg(any(rustos_boot_image, test))]
const fn scope_root(scope: MutationScope) -> u64 {
    match scope {
        MutationScope::AddressSpace(root) => root,
        MutationScope::Global => GLOBAL_SCOPE,
    }
}

#[cfg(any(rustos_boot_image, test))]
const fn shootdown_target_is_required(
    eligible: bool,
    active_root: u64,
    mutation_root: u64,
) -> bool {
    eligible && (mutation_root == GLOBAL_SCOPE || active_root == mutation_root)
}

#[cfg(any(rustos_boot_image, test))]
const fn activation_requires_cr3_write(active_root: u64, requested_root: u64) -> bool {
    active_root != requested_root
}

#[cfg(any(rustos_boot_image, test))]
fn acknowledgements_complete(
    targets: &[bool],
    acknowledgements: &[AtomicU64],
    generation: u64,
) -> bool {
    targets.len() == acknowledgements.len()
        && targets
            .iter()
            .zip(acknowledgements)
            .all(|(target, acknowledgement)| {
                !*target
                    // ORDERING: Acquire observes the target flush preceding
                    // its exact acknowledgement.
                    || acknowledgement.load(Ordering::Acquire) == generation
            })
}

fn current_cpu_index() -> usize {
    let logical_index = nucleus_core::util::lockdep::current_cpu_index();
    assert!(
        logical_index < MAX_SUPPORTED_CPUS,
        "TLB invariant: current logical CPU exceeds capacity"
    );
    logical_index
}

#[cfg(rustos_boot_image)]
fn write_cr3(root: PhysAddr) {
    let frame = PhysFrame::containing_address(root);
    // SAFETY: the root is page-aligned and owned by kernel-mm. Activation
    // excludes local interrupts; online admission additionally owns the
    // protocol lock before its mandatory parked-to-online flush.
    unsafe {
        Cr3::write(frame, Cr3Flags::empty());
    }
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::{AtomicU64, Ordering};

    use super::{
        MutationScope, acknowledgements_complete, activation_requires_cr3_write, scope_root,
        shootdown_target_is_required,
    };

    #[test]
    fn same_root_activation_preserves_tlb_but_root_change_reloads_cr3() {
        assert!(!activation_requires_cr3_write(0x4000, 0x4000));
        assert!(activation_requires_cr3_write(0, 0x4000));
        assert!(activation_requires_cr3_write(0x4000, 0x5000));
    }

    #[test]
    fn address_space_shootdown_targets_only_matching_active_roots() {
        let root = MutationScope::AddressSpace(0x4000);
        assert_eq!(scope_root(root), 0x4000);
        assert_eq!(scope_root(MutationScope::Global), 0);
        assert!(shootdown_target_is_required(true, 0x4000, 0x4000));
        assert!(!shootdown_target_is_required(true, 0x5000, 0x4000));
        assert!(shootdown_target_is_required(true, 0x5000, 0));
        assert!(!shootdown_target_is_required(false, 0x4000, 0x4000));
    }

    #[test]
    fn reclaim_requires_every_target_to_acknowledge_the_exact_generation() {
        let targets = [false, true, true];
        let acknowledgements = [AtomicU64::new(0), AtomicU64::new(7), AtomicU64::new(6)];
        assert!(!acknowledgements_complete(&targets, &acknowledgements, 7));
        // ORDERING: Release models the final target publishing its flush.
        acknowledgements[2].store(7, Ordering::Release);
        assert!(acknowledgements_complete(&targets, &acknowledgements, 7));
        assert!(!acknowledgements_complete(
            &targets[..2],
            &acknowledgements,
            7
        ));
    }
}
