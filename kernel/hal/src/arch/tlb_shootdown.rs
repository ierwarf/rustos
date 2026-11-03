//! Generation-bound x86_64 TLB shootdown and address-space activation.
//!
//! - **Owner:** `kernel-hal` owns CR3/PCID activation, the active-CPU registry,
//!   the sender-by-target request mailboxes, the paravirtual flush hint and the
//!   acknowledgement deadline.
//! - **Boundary:** `kernel-mm` must hold an `AddressSpaceMutationGuard` across
//!   every page-table edit and frame-reclaim decision.
//! - **Lifecycle:** admit a mutation (per-space stripe lock under a shared
//!   gate; global and retirement mutations take the gate exclusively) → bump
//!   the space's `tlb_gen` → publish an exact request to every CPU that is
//!   running the space (or every eligible CPU for a global mutation) → flush
//!   locally and remotely → acknowledge → permit reuse. A CPU that is not
//!   running the space owes a flush only at its next activation of it.
//! - **Concurrency:** mutations of different address spaces proceed together.
//!   Every wait a sender performs also services the requests addressed to its
//!   own CPU, so two senders waiting on each other cannot deadlock whether or
//!   not interrupts are enabled. No IRQ-time path acquires a protocol lock.
//! - **Failure:** missing eligibility, root mismatch, generation wrap, stale
//!   mailbox, or acknowledgement timeout is an immediate kernel panic.
//! - **Forbidden:** no CR3 write outside this module after SMP admission, no
//!   frame reclaim before guard completion, and no handler-side lock.
//! - **Evidence:** `tlb-shootdown-lifecycle`.
//!
//! # Delivery contract
//!
//! Every target has either performed, or durably owes before it next runs the
//! space, a flush covering this generation. Three mechanisms satisfy it and are
//! interchangeable under that one contract: an acknowledged IPI; a PCID binding
//! whose applied generation is behind the space's `tlb_gen`, which forces a
//! flush at the next activation; and the KVM paravirtual hint, where the host
//! flushes a preempted vCPU before it runs again.
//!
//! # Unacknowledged-target policy
//!
//! The supported envelope has no CPU hot-remove, so a target that does not
//! acknowledge within `ACK_LIVENESS_TIMEOUT_NS` cannot be retired and its
//! outstanding translation cannot be proven dead. The chosen policy is
//! therefore **system fail-stop**, not CPU quarantine: the watchdog panics
//! while the frame is still held behind its mutation guard. No timeout on this
//! path may ever authorize reuse, since that would free a frame a live CPU may
//! still translate.
//!
//! The deadline is a dead-owner horizon, not a latency budget. A virtualized
//! target can be descheduled by its host for a long time while remaining
//! healthy; the paravirtual hint removes exactly that case from the wait, and
//! the retry IPI carries liveness for what remains.

#[cfg(rustos_boot_image)]
use core::hint::spin_loop;
#[cfg(rustos_boot_image)]
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicUsize};
use core::sync::atomic::{AtomicU64, Ordering};

use nucleus_core::util::lockdep::{LockClass, TrackedSpinGuard, TrackedSpinLock};
use x86_64::PhysAddr;
#[cfg(rustos_boot_image)]
use x86_64::instructions::interrupts;
use x86_64::instructions::tlb;

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

/// Address spaces hash onto this many mutation locks and generation counters.
/// A collision only serializes two spaces and makes one of them flush once
/// more; it can never skip a flush.
const STRIPES: usize = 64;
/// Exclusive bit of the mutation gate; the low bits count shared holders.
const GATE_EXCLUSIVE: u64 = 1 << 63;

type ProtocolLock = TrackedSpinLock<(), { LockClass::TlbShootdown as u8 }>;
type ProtocolGuard = TrackedSpinGuard<'static, (), { LockClass::TlbShootdown as u8 }>;

/// Serializes global, retirement and admission mutations among themselves.
static GLOBAL_LOCK: ProtocolLock = TrackedSpinLock::new(());
/// Serializes mutations of the spaces that hash to one stripe.
static SPACE_LOCKS: [ProtocolLock; STRIPES] = [const { TrackedSpinLock::new(()) }; STRIPES];
/// Per-space mutations hold this shared; a global mutation holds it exclusive.
static GATE: AtomicU64 = AtomicU64::new(0);
/// `tlb_gen` of each stripe: the number of mutations whose flush is owed.
static SPACE_GEN: [AtomicU64; STRIPES] = [const { AtomicU64::new(0) }; STRIPES];
/// Each sender's own monotonic request generation; only the sender writes it.
#[cfg(rustos_boot_image)]
static SENDER_GEN: [AtomicU64; MAX_SUPPORTED_CPUS] =
    [const { AtomicU64::new(0) }; MAX_SUPPORTED_CPUS];
/// Bit `s` of `PENDING[t]` is set while sender `s` has an unserviced request
/// for target `t`.
#[cfg(rustos_boot_image)]
static PENDING: [AtomicU64; MAX_SUPPORTED_CPUS] = [const { AtomicU64::new(0) }; MAX_SUPPORTED_CPUS];
type Mailbox = [[AtomicU64; MAX_SUPPORTED_CPUS]; MAX_SUPPORTED_CPUS];
const fn mailbox() -> Mailbox {
    [const { [const { AtomicU64::new(0) }; MAX_SUPPORTED_CPUS] }; MAX_SUPPORTED_CPUS]
}
/// `[sender][target]` request payload. A request is valid once its generation
/// is published; the sender owns the slot until the target acknowledges it.
#[cfg(rustos_boot_image)]
static REQUEST_ROOT: Mailbox = mailbox();
#[cfg(rustos_boot_image)]
static REQUEST_SPACE_GEN: Mailbox = mailbox();
#[cfg(rustos_boot_image)]
static REQUEST_GENERATION: Mailbox = mailbox();
#[cfg(rustos_boot_image)]
static ACK_GENERATION: Mailbox = mailbox();

#[cfg(rustos_boot_image)]
static ACTIVE_ROOT: [AtomicU64; MAX_SUPPORTED_CPUS] =
    [const { AtomicU64::new(0) }; MAX_SUPPORTED_CPUS];
#[cfg(rustos_boot_image)]
static SHOOTDOWN_ELIGIBLE: [AtomicBool; MAX_SUPPORTED_CPUS] =
    [const { AtomicBool::new(false) }; MAX_SUPPORTED_CPUS];

// ---------------------------------------------------------------------------
// PCID: dynamic address-space identifiers with per-CPU bindings.
// ---------------------------------------------------------------------------

/// Dynamic identifiers per CPU. Identifier `k` is PCID `k + 1`; PCID 0 is the
/// one every pre-admission CR3 write used and is never bound.
#[cfg(rustos_boot_image)]
const ASID_SLOTS: usize = 6;
/// CR4.PCIDE.
#[cfg(rustos_boot_image)]
const CR4_PCIDE_BIT: u64 = 1 << 17;
/// CR3 bit 63: keep the target PCID's translations.
#[cfg(any(rustos_boot_image, test))]
const CR3_NOFLUSH: u64 = 1 << 63;

#[cfg(rustos_boot_image)]
static CPU_PCID: [AtomicBool; MAX_SUPPORTED_CPUS] =
    [const { AtomicBool::new(false) }; MAX_SUPPORTED_CPUS];
#[cfg(rustos_boot_image)]
static ASID_ROOT: [[AtomicU64; ASID_SLOTS]; MAX_SUPPORTED_CPUS] =
    [const { [const { AtomicU64::new(0) }; ASID_SLOTS] }; MAX_SUPPORTED_CPUS];
/// Space generation this CPU's tagged translations are known to include.
#[cfg(rustos_boot_image)]
static ASID_APPLIED: [[AtomicU64; ASID_SLOTS]; MAX_SUPPORTED_CPUS] =
    [const { [const { AtomicU64::new(0) }; ASID_SLOTS] }; MAX_SUPPORTED_CPUS];
#[cfg(rustos_boot_image)]
static ASID_NEXT_VICTIM: [AtomicUsize; MAX_SUPPORTED_CPUS] =
    [const { AtomicUsize::new(0) }; MAX_SUPPORTED_CPUS];

// ---------------------------------------------------------------------------
// KVM paravirtual TLB flush.
// ---------------------------------------------------------------------------

/// `MSR_KVM_STEAL_TIME`.
#[cfg(rustos_boot_image)]
const MSR_KVM_STEAL_TIME: u32 = 0x4b56_4d03;
/// CPUID.0x40000001:EAX feature bits.
#[cfg(rustos_boot_image)]
const KVM_FEATURE_STEAL_TIME: u32 = 1 << 5;
#[cfg(rustos_boot_image)]
const KVM_FEATURE_PV_TLB_FLUSH: u32 = 1 << 9;
/// The host sets this while the vCPU is scheduled out.
const KVM_VCPU_PREEMPTED: u8 = 1 << 0;
/// The guest sets this to ask the host to flush before the vCPU runs again.
const KVM_VCPU_FLUSH_TLB: u8 = 1 << 1;

/// `struct kvm_steal_time`: 64 bytes, 64-byte aligned, shared with the host,
/// which updates it asynchronously, so every field is atomic.
#[cfg(rustos_boot_image)]
#[repr(C, align(64))]
struct StealTime {
    steal: AtomicU64,
    version: AtomicU32,
    flags: AtomicU32,
    preempted: AtomicU8,
    pad: [AtomicU8; 3],
    reserved: [AtomicU32; 11],
}
#[cfg(rustos_boot_image)]
const _: [(); 64] = [(); core::mem::size_of::<StealTime>()];
#[cfg(rustos_boot_image)]
const _: [(); 16] = [(); core::mem::offset_of!(StealTime, preempted)];
#[cfg(rustos_boot_image)]
#[allow(clippy::declare_interior_mutable_const)]
const STEAL_TIME_INIT: StealTime = StealTime {
    steal: AtomicU64::new(0),
    version: AtomicU32::new(0),
    flags: AtomicU32::new(0),
    preempted: AtomicU8::new(0),
    pad: [const { AtomicU8::new(0) }; 3],
    reserved: [const { AtomicU32::new(0) }; 11],
};
#[cfg(rustos_boot_image)]
static STEAL_TIME: [StealTime; MAX_SUPPORTED_CPUS] = [STEAL_TIME_INIT; MAX_SUPPORTED_CPUS];
#[cfg(rustos_boot_image)]
static PV_FLUSH: [AtomicBool; MAX_SUPPORTED_CPUS] =
    [const { AtomicBool::new(false) }; MAX_SUPPORTED_CPUS];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MutationScope {
    AddressSpace(u64),
    Global,
}

/// What a mutation holds: its serialization lock and, for a global mutation,
/// the exclusive gate (otherwise one shared gate slot).
struct Hold {
    guard: ProtocolGuard,
    exclusive: bool,
}

pub struct AddressSpaceMutationGuard {
    scope: MutationScope,
    hold: Option<Hold>,
    restore_interrupts: bool,
}

/// A mutation whose exact shootdown quorum has completed.  Holding this value
/// retains serialization while the caller removes ownership metadata and
/// releases frames, but dropping it only unlocks the protocol; it cannot issue
/// a second shootdown for the same mutation generation.
pub struct FlushedAddressSpaceMutationGuard {
    hold: Option<Hold>,
    restore_interrupts: bool,
}

impl AddressSpaceMutationGuard {
    /// Complete a shootdown while retaining mutation serialization.
    ///
    /// Call this after removing a mapping and before releasing or reassigning
    /// any frame that a stale translation could still reach.
    pub fn flush_for_reclaim(mut self) -> FlushedAddressSpaceMutationGuard {
        assert!(
            self.hold.is_some(),
            "TLB invariant: inactive mutation guard attempted a flush"
        );
        #[cfg(rustos_boot_image)]
        finish_shootdown(self.scope, self.restore_interrupts);
        let hold = self
            .hold
            .take()
            .expect("TLB invariant: flushed mutation guard lost its protocol lock");
        let flushed = FlushedAddressSpaceMutationGuard {
            hold: Some(hold),
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
        let hold = self
            .hold
            .take()
            .expect("TLB invariant: flushed mutation guard completed twice");
        release(hold, self.restore_interrupts);
    }
}

pub fn begin_address_space_mutation(root: PhysAddr) -> AddressSpaceMutationGuard {
    let root = root.as_u64();
    assert!(
        root != 0 && root.is_multiple_of(PAGE_SIZE),
        "TLB invariant: address-space mutation root is invalid"
    );
    let (hold, restore_interrupts) = acquire_space(root);
    AddressSpaceMutationGuard {
        scope: MutationScope::AddressSpace(root),
        hold: Some(hold),
        restore_interrupts,
    }
}

/// Attempts to enter the address-space mutation protocol from an
/// interrupt-disabled synchronous exception path.
///
/// A page fault must not wait for a mutation gate or stripe: it would stall a
/// CPU that a sender may be waiting on. A single try is safe. Failure means the
/// faulting instruction must be restarted so `iretq` restores interrupts and
/// lets the in-flight work finish. Success needs no further proof of absence,
/// because every wait a sender performs services requests addressed to its own
/// CPU, so interrupts staying clear cannot hold up another sender.
pub fn try_begin_address_space_mutation_from_exception(
    root: PhysAddr,
) -> Option<AddressSpaceMutationGuard> {
    let root = root.as_u64();
    assert!(
        root != 0 && root.is_multiple_of(PAGE_SIZE),
        "TLB invariant: exception-time mutation root is invalid"
    );
    #[cfg(rustos_boot_image)]
    {
        assert!(
            !interrupts::are_enabled(),
            "TLB invariant: exception-time mutation entered with interrupts enabled"
        );
    }
    let hold = try_space(root)?;
    Some(AddressSpaceMutationGuard {
        scope: MutationScope::AddressSpace(root),
        hold: Some(hold),
        restore_interrupts: false,
    })
}

pub fn begin_global_mapping_mutation() -> AddressSpaceMutationGuard {
    let (hold, restore_interrupts) = acquire_global();
    AddressSpaceMutationGuard {
        scope: MutationScope::Global,
        hold: Some(hold),
        restore_interrupts,
    }
}

/// Hold the global gate from the final inactive-root check through the
/// global shootdown completed by the returned guard.
pub fn begin_address_space_retirement(root: PhysAddr) -> AddressSpaceMutationGuard {
    let root_raw = root.as_u64();
    assert!(
        root_raw != 0 && root_raw.is_multiple_of(PAGE_SIZE),
        "TLB invariant: retired address-space root is invalid"
    );
    let (hold, restore_interrupts) = acquire_global();
    #[cfg(rustos_boot_image)]
    assert_address_space_inactive_locked(root_raw);
    AddressSpaceMutationGuard {
        scope: MutationScope::Global,
        hold: Some(hold),
        restore_interrupts,
    }
}

impl Drop for AddressSpaceMutationGuard {
    fn drop(&mut self) {
        assert!(
            self.hold.is_some(),
            "TLB invariant: mutation guard completed twice"
        );
        #[cfg(rustos_boot_image)]
        finish_shootdown(self.scope, self.restore_interrupts);
        #[cfg(not(rustos_boot_image))]
        let _ = (self.scope, self.restore_interrupts);
        let hold = self
            .hold
            .take()
            .expect("TLB invariant: mutation guard lost its protocol lock");
        release(hold, self.restore_interrupts);
    }
}

/// The stripe an address space hashes onto.
const fn stripe(root: u64) -> usize {
    // Roots are page aligned; mix the frame number so consecutive frames do
    // not walk the stripes in lockstep with allocation order.
    let frame = root >> 12;
    ((frame ^ (frame >> 6) ^ (frame >> 12)) as usize) % STRIPES
}

/// Gate transition for a per-space mutation entering shared mode.
#[cfg(any(rustos_boot_image, test))]
const fn gate_enter_shared(gate: u64) -> Option<u64> {
    if gate & GATE_EXCLUSIVE != 0 || gate == GATE_EXCLUSIVE - 1 {
        None
    } else {
        Some(gate + 1)
    }
}

/// One non-blocking attempt at a per-space mutation: the shared gate, then the
/// stripe lock. A failed stripe attempt gives the gate slot back.
fn try_space(root: u64) -> Option<Hold> {
    #[cfg(rustos_boot_image)]
    {
        // ORDERING: Acquire observes a global mutation's exclusive release;
        // the AcqRel CAS publishes this holder to a draining global mutation.
        let gate = GATE.load(Ordering::Acquire);
        let next = gate_enter_shared(gate)?;
        GATE.compare_exchange(gate, next, Ordering::AcqRel, Ordering::Acquire)
            .ok()?;
        match SPACE_LOCKS[stripe(root)].try_lock() {
            Some(guard) => Some(Hold {
                guard,
                exclusive: false,
            }),
            None => {
                // ORDERING: Release returns the slot to a draining global
                // mutation only after this attempt touched nothing.
                GATE.fetch_sub(1, Ordering::Release);
                None
            }
        }
    }
    #[cfg(not(rustos_boot_image))]
    {
        SPACE_LOCKS[stripe(root)].try_lock().map(|guard| Hold {
            guard,
            exclusive: false,
        })
    }
}

/// Block until a per-space mutation is admitted. Returns whether interrupts
/// were enabled on entry and must be restored by `release`.
fn acquire_space(root: u64) -> (Hold, bool) {
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
            if let Some(hold) = try_space(root) {
                return (hold, restore_interrupts);
            }
            if restore_interrupts {
                // A sender holding the gate or the stripe may need this CPU's
                // acknowledgement; keep the interrupt window open between
                // attempts so contention cannot suppress the IPI that lets
                // that sender finish.
                interrupts::enable();
            }
            // A caller that entered with interrupts off cannot take the IPI,
            // so it answers its own requests here instead.
            service_requests();
            if deadline.elapsed_nanos() >= PROTOCOL_ACQUIRE_TIMEOUT_NS {
                panic!("TLB invariant: protocol lock acquisition timed out");
            }
            spin_loop();
        }
    }
    #[cfg(not(rustos_boot_image))]
    {
        (
            Hold {
                guard: SPACE_LOCKS[stripe(root)].lock(),
                exclusive: false,
            },
            false,
        )
    }
}

/// Block until a global mutation holds the gate exclusively: no per-space
/// mutation is in flight and none is admitted until release.
fn acquire_global() -> (Hold, bool) {
    #[cfg(rustos_boot_image)]
    {
        assert_eq!(
            nucleus_core::util::lockdep::irq_context_depth(),
            0,
            "TLB invariant: mutation protocol entered from IRQ context"
        );
        let restore_interrupts = interrupts::are_enabled();
        let mut deadline = super::clock::SpinDeadline::start();
        let guard = loop {
            if restore_interrupts {
                interrupts::disable();
            }
            if let Some(guard) = GLOBAL_LOCK.try_lock() {
                break guard;
            }
            if restore_interrupts {
                interrupts::enable();
            }
            service_requests();
            if deadline.elapsed_nanos() >= PROTOCOL_ACQUIRE_TIMEOUT_NS {
                panic!("TLB invariant: protocol lock acquisition timed out");
            }
            spin_loop();
        };
        // Only this CPU holds `GLOBAL_LOCK`, so the exclusive bit is ours alone.
        // ORDERING: AcqRel publishes the bit to entering per-space mutations.
        GATE.fetch_or(GATE_EXCLUSIVE, Ordering::AcqRel);
        // Drain in-flight per-space mutations. They may be waiting for this
        // CPU's acknowledgement, so the drain keeps servicing requests and, when
        // the caller permits, an interrupt window.
        loop {
            // ORDERING: Acquire pairs with each holder's Release decrement.
            if GATE.load(Ordering::Acquire) == GATE_EXCLUSIVE {
                break;
            }
            if restore_interrupts {
                interrupts::enable();
                spin_loop();
                interrupts::disable();
            }
            service_requests();
            if deadline.elapsed_nanos() >= PROTOCOL_ACQUIRE_TIMEOUT_NS {
                panic!("TLB invariant: protocol drain timed out");
            }
            spin_loop();
        }
        (
            Hold {
                guard,
                exclusive: true,
            },
            restore_interrupts,
        )
    }
    #[cfg(not(rustos_boot_image))]
    {
        (
            Hold {
                guard: GLOBAL_LOCK.lock(),
                exclusive: true,
            },
            false,
        )
    }
}

fn release(hold: Hold, restore_interrupts: bool) {
    let Hold { guard, exclusive } = hold;
    #[cfg(rustos_boot_image)]
    {
        if exclusive {
            // Keep GLOBAL_LOCK until the exclusive bit is clear. Otherwise a
            // succeeding global owner can set the same bit before this owner
            // clears it, admitting shared mutations into its critical section.
            // ORDERING: Release publishes the finished global mutation to every
            // per-space mutation admitted after the bit clears.
            GATE.fetch_and(!GATE_EXCLUSIVE, Ordering::Release);
            drop(guard);
        } else {
            drop(guard);
            // ORDERING: Release publishes this shared mutation before a global
            // owner's gate drain observes the count reaching zero.
            GATE.fetch_sub(1, Ordering::Release);
        }
        if restore_interrupts {
            // The matching acquisition disabled local interrupts only after
            // recording that they were enabled on entry.
            interrupts::enable();
        }
    }
    #[cfg(not(rustos_boot_image))]
    {
        drop(guard);
        let _ = (exclusive, restore_interrupts);
    }
}

// ---------------------------------------------------------------------------
// Activation.
// ---------------------------------------------------------------------------

/// The CR3 image for one PCID.
#[cfg(any(rustos_boot_image, test))]
const fn cr3_image(root: u64, pcid: u64, keep_translations: bool) -> u64 {
    root | pcid | if keep_translations { CR3_NOFLUSH } else { 0 }
}

/// Whether a binding's tagged translations may be kept across an activation.
#[cfg(any(rustos_boot_image, test))]
const fn binding_is_current(bound: bool, applied_generation: u64, space_generation: u64) -> bool {
    bound && applied_generation >= space_generation
}

/// Load one address-space root and publish it as this CPU's active root.
///
/// This path runs in timer/software-schedule IRQ context and therefore must
/// never wait for a mutation lock.
///
/// Without PCID a changed activation writes CR3 (discarding every non-global
/// translation) before release-publishing the new root, so a sender either
/// observes the old matching root and targets it, or a root whose preceding CR3
/// write already discarded translations.
///
/// With PCID translations survive a switch, so the order inverts: the root is
/// published first and the space's `tlb_gen` is read afterwards. A sender bumps
/// the generation before it samples roots, hence either it sees this CPU as
/// running the space and sends the request, or this CPU reads the new
/// generation, finds its binding behind, and flushes. Both orders cover a
/// mutation concurrent with the switch.
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
        let active_root = ACTIVE_ROOT[logical_index].load(Ordering::Acquire);
        if !activation_requires_cr3_write(active_root, root_raw) {
            return;
        }
        if CPU_PCID[logical_index].load(Ordering::Relaxed) {
            activate_tagged(logical_index, root_raw);
            return;
        }
        write_cr3_raw(root_raw);
        // ORDERING: Release publishes the active root only after CR3 accepted
        // the same frame. A sender's Acquire may use this value for exact-root
        // target selection only because every changed untagged activation
        // flushes via CR3 before this publication.
        ACTIVE_ROOT[logical_index].store(root_raw, Ordering::Release);
    });
}

#[cfg(rustos_boot_image)]
fn activate_tagged(logical_index: usize, root_raw: u64) {
    // ORDERING: SeqCst on the publication and the fence below make this store
    // precede the generation read in the single total order a sender's
    // generation bump and root sampling also participate in.
    ACTIVE_ROOT[logical_index].store(root_raw, Ordering::SeqCst);
    core::sync::atomic::fence(Ordering::SeqCst);
    // ORDERING: SeqCst; read after the root publication above.
    let generation = SPACE_GEN[stripe(root_raw)].load(Ordering::SeqCst);
    let bound = (0..ASID_SLOTS)
        .find(|slot| ASID_ROOT[logical_index][*slot].load(Ordering::Relaxed) == root_raw);
    let keep = binding_is_current(
        bound.is_some(),
        bound.map_or(0, |slot| {
            ASID_APPLIED[logical_index][slot].load(Ordering::Relaxed)
        }),
        generation,
    );
    let slot = bound.unwrap_or_else(|| {
        let victim = ASID_NEXT_VICTIM[logical_index].load(Ordering::Relaxed) % ASID_SLOTS;
        ASID_NEXT_VICTIM[logical_index].store((victim + 1) % ASID_SLOTS, Ordering::Relaxed);
        ASID_ROOT[logical_index][victim].store(root_raw, Ordering::Relaxed);
        victim
    });
    // Interrupts are off, so no request lands between this record and the CR3
    // write that makes it true.
    ASID_APPLIED[logical_index][slot].store(generation, Ordering::Relaxed);
    write_cr3_raw(cr3_image(root_raw, slot as u64 + 1, keep));
}

/// Whether the hardware can run tagged address spaces under this design.
#[cfg(rustos_boot_image)]
fn pcid_supported() -> bool {
    // SAFETY: CPUID is unprivileged and side-effect free.
    #[allow(unused_unsafe)]
    let (leaf1, leaf7) = unsafe {
        (
            core::arch::x86_64::__cpuid(1),
            core::arch::x86_64::__cpuid_count(7, 0),
        )
    };
    let pcid = leaf1.ecx & (1 << 17) != 0;
    let invpcid = leaf7.ebx & (1 << 10) != 0;
    pcid && invpcid
}

#[cfg(rustos_boot_image)]
fn read_cr4() -> u64 {
    let cr4: u64;
    // SAFETY: reading CR4 has no side effects and takes no operand.
    unsafe {
        core::arch::asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack, preserves_flags));
    }
    cr4
}

/// `INVPCID` types.
#[cfg(rustos_boot_image)]
#[derive(Clone, Copy)]
enum Invpcid {
    /// One PCID, global entries retained.
    SingleContext(u64),
    /// Every PCID including global entries.
    AllContexts,
}

#[cfg(rustos_boot_image)]
fn invpcid(kind: Invpcid) {
    let (ty, pcid): (u64, u64) = match kind {
        Invpcid::SingleContext(pcid) => (1, pcid),
        Invpcid::AllContexts => (2, 0),
    };
    let descriptor: [u64; 2] = [pcid, 0];
    // SAFETY: CR4.PCIDE is set and CPUID reported INVPCID before this CPU was
    // admitted; the descriptor is a valid 128-bit operand for types 1 and 2.
    unsafe {
        core::arch::asm!(
            "invpcid {ty}, [{desc}]",
            ty = in(reg) ty,
            desc = in(reg) descriptor.as_ptr(),
            options(nostack, preserves_flags)
        );
    }
}

/// Discard this CPU's translations that a request covers, then record how far
/// the space is applied. Runs on the target (IPI or polling) and on the sender.
#[cfg(rustos_boot_image)]
fn invalidate_local(logical_index: usize, root: u64, space_generation: u64) {
    // ORDERING: Acquire pairs with the Release that published tagged mode after its CR3 load.
    if !CPU_PCID[logical_index].load(Ordering::Acquire) {
        tlb::flush_all();
        return;
    }
    if root == GLOBAL_SCOPE {
        // Read the generations before the flush: a bump that lands after the
        // read is not covered by it and must stay owed.
        let mut seen = [0u64; ASID_SLOTS];
        for (slot, value) in seen.iter_mut().enumerate() {
            let bound = ASID_ROOT[logical_index][slot].load(Ordering::Relaxed);
            *value = if bound == 0 {
                0
            } else {
                // ORDERING: SeqCst reads the generation in the total order a mutation bumps it in; read before the flush so a later bump stays owed.
                SPACE_GEN[stripe(bound)].load(Ordering::SeqCst)
            };
        }
        invpcid(Invpcid::AllContexts);
        for (slot, value) in seen.iter().enumerate() {
            ASID_APPLIED[logical_index][slot].fetch_max(*value, Ordering::Relaxed);
        }
        return;
    }
    for slot in 0..ASID_SLOTS {
        if ASID_ROOT[logical_index][slot].load(Ordering::Relaxed) == root {
            invpcid(Invpcid::SingleContext(slot as u64 + 1));
            ASID_APPLIED[logical_index][slot].fetch_max(space_generation, Ordering::Relaxed);
        }
    }
}

/// Complete the pre-dispatch admission barrier for the current CPU.
///
/// An AP is not a shootdown target before this call. Enabling tagged address
/// spaces and reloading CR3 immediately before eligibility closes the
/// parked-to-online window without requiring a generation token in the
/// no-payload reschedule protocol.
pub fn admit_current_cpu_online() {
    #[cfg(rustos_boot_image)]
    {
        let (hold, restore_interrupts) = acquire_global();
        let logical_index = current_cpu_index();
        // ORDERING: Acquire observes the root published by activation.
        let root = ACTIVE_ROOT[logical_index].load(Ordering::Acquire);
        assert_ne!(
            root, 0,
            "TLB invariant: CPU admitted online without an active root"
        );
        if pcid_supported() {
            enable_tagged_address_spaces(logical_index, root);
        } else {
            write_cr3_raw(root);
        }
        // ORDERING: Release publishes eligibility only after the admission
        // reload discarded every translation acquired while parked.
        assert!(
            SHOOTDOWN_ELIGIBLE[logical_index]
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
            "TLB invariant: CPU admitted to shootdowns twice"
        );
        release(hold, restore_interrupts);
    }
}

#[cfg(rustos_boot_image)]
fn enable_tagged_address_spaces(logical_index: usize, root: u64) {
    assert_eq!(
        read_cr4() & CR4_PCIDE_BIT,
        0,
        "TLB invariant: CR4.PCIDE was already set at admission"
    );
    // SAFETY: CR4.PCIDE may be set only while CR3[11:0] is zero, which every
    // pre-admission CR3 write guaranteed; PCID and INVPCID were checked.
    unsafe {
        core::arch::asm!(
            "mov {tmp}, cr4",
            "or {tmp}, {bit}",
            "mov cr4, {tmp}",
            tmp = out(reg) _,
            bit = in(reg) CR4_PCIDE_BIT,
            options(nostack)
        );
    }
    // ORDERING: SeqCst; admission records the generation this first binding is current with.
    let generation = SPACE_GEN[stripe(root)].load(Ordering::SeqCst);
    ASID_ROOT[logical_index][0].store(root, Ordering::Relaxed);
    ASID_APPLIED[logical_index][0].store(generation, Ordering::Relaxed);
    ASID_NEXT_VICTIM[logical_index].store(1, Ordering::Relaxed);
    // Flush the entire TLB, including entries tagged with the pre-admission
    // PCID 0, then load the root under its first dynamic PCID.
    invpcid(Invpcid::AllContexts);
    write_cr3_raw(cr3_image(root, 1, false));
    // ORDERING: Release publishes the mode only after the root is loaded in it.
    CPU_PCID[logical_index].store(true, Ordering::Release);
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
        let (hold, restore_interrupts) = acquire_global();
        assert_address_space_inactive_locked(root_raw);
        release(hold, restore_interrupts);
    }
}

#[cfg(rustos_boot_image)]
fn assert_address_space_inactive_locked(root_raw: u64) {
    for logical_index in 0..MAX_SUPPORTED_CPUS {
        // ORDERING: Acquire pairs with activation publication and online
        // admission while the exclusive gate excludes a concurrent mutation.
        if SHOOTDOWN_ELIGIBLE[logical_index].load(Ordering::Acquire)
            && ACTIVE_ROOT[logical_index].load(Ordering::Acquire) == root_raw
        {
            panic!(
                "TLB invariant: address-space root {root_raw:#x} reclaimed while active on logical CPU {logical_index}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Paravirtual flush.
// ---------------------------------------------------------------------------

/// Offer this CPU's steal-time area to a KVM host so the host reports when the
/// vCPU is preempted and can flush its TLB on resume. Feature-detected: on any
/// other hypervisor, or bare metal, this does nothing and shootdowns use IPIs.
///
/// `translate` maps a kernel virtual address to its guest-physical address.
pub fn enable_paravirtual_flush(translate: fn(u64) -> u64) {
    #[cfg(rustos_boot_image)]
    {
        let logical_index = current_cpu_index();
        if !kvm_paravirtual_flush_offered() {
            return;
        }
        let area = &STEAL_TIME[logical_index] as *const StealTime as u64;
        let physical = translate(area);
        assert!(
            physical.is_multiple_of(64),
            "TLB invariant: steal-time area is not 64-byte aligned"
        );
        // SAFETY: the CPUID leaf advertised steal time; bit 0 enables it and
        // the area is a static the host may write for the rest of the boot.
        unsafe {
            x86_64::registers::model_specific::Msr::new(MSR_KVM_STEAL_TIME).write(physical | 1);
        }
        // ORDERING: Release publishes the enabled state after the MSR write.
        PV_FLUSH[logical_index].store(true, Ordering::Release);
    }
    #[cfg(not(rustos_boot_image))]
    let _ = translate;
}

#[cfg(rustos_boot_image)]
fn kvm_paravirtual_flush_offered() -> bool {
    // SAFETY: CPUID is unprivileged and side-effect free.
    #[allow(unused_unsafe)]
    let (leaf1, hypervisor) = unsafe {
        (
            core::arch::x86_64::__cpuid(1),
            core::arch::x86_64::__cpuid(0x4000_0000),
        )
    };
    if leaf1.ecx & (1 << 31) == 0 {
        return false;
    }
    let kvm = hypervisor.ebx == 0x4b4d_564b
        && hypervisor.ecx == 0x564b_4d56
        && hypervisor.edx == 0x4d
        && hypervisor.eax >= 0x4000_0001;
    if !kvm {
        return false;
    }
    // SAFETY: as above, a supported hypervisor leaf.
    #[allow(unused_unsafe)]
    let features = unsafe { core::arch::x86_64::__cpuid(0x4000_0001) }.eax;
    features & KVM_FEATURE_STEAL_TIME != 0 && features & KVM_FEATURE_PV_TLB_FLUSH != 0
}

/// Whether a mutation of this scope may leave a preempted target to its host.
///
/// Address-space mutations free frames that a target's IRQ-off software walk
/// (the lock-free fault walk) may still be about to read; a host flush on
/// resume does not exclude that, an acknowledgement does. Global and retirement
/// mutations free no frame reachable by such a walk.
#[cfg(any(rustos_boot_image, test))]
const fn paravirtual_skip_admissible(mutation_root: u64) -> bool {
    mutation_root == GLOBAL_SCOPE
}

/// Ask the host to flush a preempted vCPU instead of interrupting it.
///
/// Returns true when the target was marked: the host flushes its TLB before the
/// vCPU runs again, so the sender need not wait for it. A vCPU that is running,
/// or halted rather than preempted, is not marked and gets the IPI.
#[cfg(any(rustos_boot_image, test))]
fn paravirtual_flush_claimed(preempted: &core::sync::atomic::AtomicU8) -> bool {
    // ORDERING: Acquire observes the host's preempted byte before deciding to claim a flush.
    let state = preempted.load(Ordering::Acquire);
    state & KVM_VCPU_PREEMPTED != 0
        && preempted
            .compare_exchange(
                state,
                state | KVM_VCPU_FLUSH_TLB,
                // ORDERING: AcqRel claims the host flush before the sender stops waiting for this target.
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
}

// ---------------------------------------------------------------------------
// The shootdown.
// ---------------------------------------------------------------------------

#[cfg(rustos_boot_image)]
fn finish_shootdown(scope: MutationScope, caller_had_interrupts: bool) {
    let current = current_cpu_index();
    let mutation_root = scope_root(scope);
    // The space's generation moves before any root is sampled, so an activation
    // racing with this mutation either is seen below or reads the new
    // generation and flushes.
    let space_generation = match scope {
        MutationScope::AddressSpace(root) => {
            // ORDERING: SeqCst pairs with the activation's published root and
            // generation read; see `activate_address_space`.
            SPACE_GEN[stripe(root)].fetch_add(1, Ordering::SeqCst) + 1
        }
        MutationScope::Global => 0,
    };
    // ORDERING: SeqCst fence between the generation bump and the root samples.
    core::sync::atomic::fence(Ordering::SeqCst);
    let Some(topology) = super::acpi::cpu_topology() else {
        tlb::flush_all();
        return;
    };
    // ORDERING: only this sender writes its counter.
    let generation = SENDER_GEN[current]
        .load(Ordering::Relaxed)
        .checked_add(1)
        .expect("TLB invariant: shootdown generation wrapped");
    SENDER_GEN[current].store(generation, Ordering::Relaxed);

    let mut targets = [false; MAX_SUPPORTED_CPUS];
    for descriptor in topology.cpus() {
        let target = usize::from(descriptor.logical_index);
        // ORDERING: Acquire observes online admission before target selection.
        let eligible = SHOOTDOWN_ELIGIBLE[target].load(Ordering::Acquire);
        // ORDERING: SeqCst pairs with the activation publication above.
        let active_root = ACTIVE_ROOT[target].load(Ordering::SeqCst);
        if target == current || !shootdown_target_is_required(eligible, active_root, mutation_root)
        {
            continue;
        }
        assert_ne!(
            active_root, 0,
            "TLB invariant: eligible target has no active root"
        );
        // A preempted vCPU is flushed by its host before it runs again; the
        // edit being flushed for happened before this point. Only a global or
        // retirement mutation may use this: an address-space mutation frees
        // frames that an IRQ-off software walk on the target may be about to
        // read, and only the target's acknowledgement (which needs it to leave
        // that section) excludes that walk. The host's flush does not.
        if paravirtual_skip_admissible(mutation_root)
            // ORDERING: Acquire pairs with the Release published after the steal-time MSR write.
            && PV_FLUSH[target].load(Ordering::Acquire)
            && paravirtual_flush_claimed(&STEAL_TIME[target].preempted)
        {
            continue;
        }
        // ORDERING: These Relaxed payload fields are not authority on their
        // own; the following Release request generation publishes them all.
        REQUEST_ROOT[current][target].store(mutation_root, Ordering::Relaxed);
        REQUEST_SPACE_GEN[current][target].store(space_generation, Ordering::Relaxed);
        // ORDERING: Release publishes the complete request slot.
        REQUEST_GENERATION[current][target].store(generation, Ordering::Release);
        // ORDERING: Release orders the slot before the target can see the bit.
        PENDING[target].fetch_or(1 << current, Ordering::Release);
        targets[target] = true;
    }

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

    // The sender flushes unconditionally because it owns the page-table edit
    // even when its current root changed during the mutation transaction. It
    // does so after the IPIs are out, so its own flush overlaps the targets'
    // (Amit et al., concurrent local and remote flush); reclaim still waits for
    // every acknowledgement below.
    invalidate_local(current, mutation_root, space_generation);

    if let Some(class) =
        nucleus_core::util::lockdep::first_held_spin_class_except(LockClass::TlbShootdown as u8)
    {
        note_wait_under_lock(class);
    }
    // One batched deadline spans the complete acknowledgement quorum, so the
    // liveness horizon still covers the whole shootdown rather than resetting
    // per target. The wait services this CPU's own requests, so two senders
    // that target each other cannot deadlock, and opens an interrupt window when
    // the caller had interrupts enabled so the IPI path is live too.
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
            if ACK_GENERATION[current][target].load(Ordering::Acquire) == generation {
                break;
            }
            service_requests();
            let elapsed = deadline.elapsed_nanos();
            if elapsed >= ACK_LIVENESS_TIMEOUT_NS {
                // The panic text below is lost when another CPU, blocked behind
                // the lock this CPU still holds, panics concurrently and prints
                // a nested panic. A milestone survives that.
                crate::debug::record_milestone(
                    crate::debug::LogCategory::Sched,
                    "tlb-ack-liveness-timeout",
                    (u64::from(descriptor.logical_index) << 32) | u64::from(descriptor.apic_id),
                    generation,
                );
                panic!(
                    "TLB liveness watchdog: generation {generation} retained reclaim quarantine while logical CPU {} APIC {} failed to acknowledge after {}ns",
                    descriptor.logical_index,
                    descriptor.apic_id,
                    deadline.exact_elapsed_nanos()
                );
            }
            if elapsed >= retry_after {
                // The slot remains generation-owned and the frame remains
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
            if caller_had_interrupts {
                // The mutation disabled interrupts at admission; open a window
                // so a timer or reschedule IPI is not held off for the wait.
                interrupts::enable();
                spin_loop();
                interrupts::disable();
            } else {
                spin_loop();
            }
        }
    }
    let mut acknowledgements = [0u64; MAX_SUPPORTED_CPUS];
    for (target, slot) in acknowledgements.iter_mut().enumerate() {
        // ORDERING: Acquire observes each target flush preceding its exact acknowledgement.
        *slot = ACK_GENERATION[current][target].load(Ordering::Acquire);
    }
    assert!(
        acknowledgements_complete(&targets, &acknowledgements, generation),
        "TLB invariant: reclaim reached without every exact acknowledgement"
    );
}

/// Records, once per lock class, that a shootdown acknowledgement wait began
/// while the sender held another raw lock. Bounded, so it costs one bit test.
#[cfg(rustos_boot_image)]
fn note_wait_under_lock(class: u8) {
    static SEEN: AtomicU64 = AtomicU64::new(0);
    let bit = 1u64 << (class & 63);
    // ORDERING: Relaxed; a once-per-class telemetry latch.
    if SEEN.fetch_or(bit, Ordering::Relaxed) & bit == 0 {
        crate::debug::record_milestone(
            crate::debug::LogCategory::Sched,
            "tlb-wait-under-lock",
            u64::from(class),
            0,
        );
    }
}

/// Answer every request addressed to this CPU. Called by the IPI leaf and by
/// each wait a sender performs; either may run first and the other finds the
/// pending set empty, so a duplicate IPI is harmless.
#[cfg(rustos_boot_image)]
fn service_requests() {
    let logical_index = current_cpu_index();
    // ORDERING: Acquire observes each sender's complete slot behind its bit.
    let mut pending = PENDING[logical_index].swap(0, Ordering::Acquire);
    while pending != 0 {
        let sender = pending.trailing_zeros() as usize;
        pending &= pending - 1;
        // ORDERING: Acquire pairs with the sender's Release generation.
        let generation = REQUEST_GENERATION[sender][logical_index].load(Ordering::Acquire);
        assert_ne!(
            generation, 0,
            "TLB invariant: target received an unpublished shootdown"
        );
        let request_root = REQUEST_ROOT[sender][logical_index].load(Ordering::Relaxed);
        assert!(
            request_root == GLOBAL_SCOPE || request_root.is_multiple_of(PAGE_SIZE),
            "TLB invariant: runtime shootdown scope is invalid"
        );
        let space_generation = REQUEST_SPACE_GEN[sender][logical_index].load(Ordering::Relaxed);
        invalidate_local(logical_index, request_root, space_generation);
        // ORDERING: Release publishes flush completion for the exact generation.
        ACK_GENERATION[sender][logical_index].store(generation, Ordering::Release);
    }
}

pub(super) fn on_interrupt() {
    #[cfg(rustos_boot_image)]
    service_requests();
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
fn acknowledgements_complete(targets: &[bool], acknowledgements: &[u64], generation: u64) -> bool {
    targets.len() == acknowledgements.len()
        && targets
            .iter()
            .zip(acknowledgements)
            .all(|(target, acknowledgement)| !*target || *acknowledgement == generation)
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
fn write_cr3_raw(image: u64) {
    // SAFETY: the root is page-aligned and owned by kernel-mm. Activation
    // excludes local interrupts; online admission additionally owns the
    // exclusive gate before its mandatory parked-to-online flush. With
    // CR4.PCIDE clear the PCID and no-flush bits of `image` are zero.
    unsafe {
        core::arch::asm!("mov cr3, {}", in(reg) image, options(nostack, preserves_flags));
    }
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::{AtomicU8, Ordering};

    use super::{
        CR3_NOFLUSH, GATE_EXCLUSIVE, KVM_VCPU_FLUSH_TLB, KVM_VCPU_PREEMPTED, MutationScope,
        STRIPES, acknowledgements_complete, activation_requires_cr3_write, binding_is_current,
        cr3_image, gate_enter_shared, paravirtual_flush_claimed, paravirtual_skip_admissible,
        scope_root, shootdown_target_is_required, stripe,
    };

    #[test]
    fn global_gate_is_cleared_before_releasing_its_owner_lock() {
        let source = include_str!("tlb_shootdown.rs");
        let release = source.split_once("fn release(hold: Hold").unwrap().1
            .split_once("// Activation.").unwrap().0;
        let exclusive = release.split_once("if exclusive {").unwrap().1
            .split_once("} else {").unwrap().0;
        assert!(exclusive.find("GATE.fetch_and").unwrap() < exclusive.find("drop(guard)").unwrap());
    }

    #[test]
    fn same_root_activation_preserves_tlb_but_root_change_reloads_cr3() {
        assert!(!activation_requires_cr3_write(0x4000, 0x4000));
        assert!(activation_requires_cr3_write(0, 0x4000));
        assert!(activation_requires_cr3_write(0x4000, 0x5000));
    }

    #[test]
    fn tagged_activation_keeps_translations_only_for_a_current_binding() {
        // A missing binding, or one behind the space's generation, must flush.
        assert!(!binding_is_current(false, 9, 3));
        assert!(!binding_is_current(true, 2, 3));
        assert!(binding_is_current(true, 3, 3));
        assert!(binding_is_current(true, 4, 3));
        assert_eq!(cr3_image(0x4000, 2, true), 0x4000 | 2 | CR3_NOFLUSH);
        assert_eq!(cr3_image(0x4000, 2, false), 0x4002);
        assert_eq!(CR3_NOFLUSH, 1 << 63);
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
        let mut acknowledgements = [0u64, 7, 6];
        assert!(!acknowledgements_complete(&targets, &acknowledgements, 7));
        acknowledgements[2] = 7;
        assert!(acknowledgements_complete(&targets, &acknowledgements, 7));
        assert!(!acknowledgements_complete(
            &targets[..2],
            &acknowledgements,
            7
        ));
    }

    #[test]
    fn the_gate_admits_shared_holders_until_a_global_mutation_takes_it() {
        assert_eq!(gate_enter_shared(0), Some(1));
        assert_eq!(gate_enter_shared(5), Some(6));
        assert_eq!(gate_enter_shared(GATE_EXCLUSIVE), None);
        assert_eq!(gate_enter_shared(GATE_EXCLUSIVE | 3), None);
        assert_eq!(gate_enter_shared(GATE_EXCLUSIVE - 1), None);
    }

    #[test]
    fn stripes_are_in_range_and_page_neighbours_do_not_share_one() {
        for frame in 0..4096u64 {
            assert!(stripe(frame << 12) < STRIPES);
        }
        let mut used = [false; STRIPES];
        for frame in 0..STRIPES as u64 {
            used[stripe((0x1000 + frame) << 12)] = true;
        }
        assert!(used.iter().filter(|used| **used).count() > STRIPES / 2);
    }

    #[test]
    fn only_global_and_retirement_mutations_may_skip_a_preempted_target() {
        // An address-space mutation frees frames an IRQ-off walk on the target
        // may still read, so it must wait for that target's acknowledgement.
        assert!(paravirtual_skip_admissible(0));
        assert!(!paravirtual_skip_admissible(0x4000));
    }

    #[test]
    fn the_paravirtual_hint_marks_only_a_preempted_vcpu_and_only_once() {
        let running = AtomicU8::new(0);
        assert!(!paravirtual_flush_claimed(&running));
        assert_eq!(running.load(Ordering::Relaxed), 0);

        let preempted = AtomicU8::new(KVM_VCPU_PREEMPTED);
        assert!(paravirtual_flush_claimed(&preempted));
        assert_eq!(
            preempted.load(Ordering::Relaxed),
            KVM_VCPU_PREEMPTED | KVM_VCPU_FLUSH_TLB
        );
        // The flag is idempotent for a second sender.
        assert!(paravirtual_flush_claimed(&preempted));

        // Once the host resumed the vCPU and cleared the byte, it gets an IPI.
        preempted.store(0, Ordering::Relaxed);
        assert!(!paravirtual_flush_claimed(&preempted));
    }
}
