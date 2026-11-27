//! Per-CPU paired execution descriptors for register-sized synchronous IPC (Track L2).
//!
//! - **Owner:** each logical CPU owns at most one [`PairControl`] / `PairImage`
//!   holding an executing (`Running`) task and an inactive paired (`Paired`) partner.
//! - **Boundary:** scheduler custody remains authoritative in `RunOwnerWord`; both
//!   tasks retain exact `StackPin`, `ExecutionPermit`, `PlacementGrant`, and
//!   `PublishedFrameView` across paired switches without slab or `ResidentCell` churn.
//! - **Lifecycle:** `Vacant -> Preparing -> Live -> Breaking -> Vacant`. Cold pair
//!   formation claims the partner into `Paired` first while `Preparing`, aborts cleanly
//!   back to `Blocked` if a break/race arrives before `Live` commits, and publishes a
//!   single `Live` state once both members' leases are safely held.
//! - **Concurrency:** remote observers (`retire_slot`, `conservation`, `wake_publication`,
//!   `ipc_handoff`, affinity migration) read `PAIR_PARTNER` / `PAIR_STATES` lock-free and
//!   post exact-generation break requests; only the owning CPU (with IRQs masked under
//!   `DispatchPreemption`) mutates `PairImage` or dissolves the pair.
//! - **Failure:** any mismatch, affinity exclusion, pending activation/FIFO work, or
//!   break request dissolves the pair cleanly back to canonical `Blocked` (with exact
//!   `wait_reason`) or `Local` (`WakePartner`) and drops retained leases.
//! - **Forbidden:** no direct owner-word overwrite bypassing `pair_claim` /
//!   `pair_park` / `pair_resume`, no cross-CPU mutation of another CPU's pair slot,
//!   and no leaving a paired partner without exact conservation accounting.
//! - **Evidence:** `scheduler-owner-word/SchedulerOwnerWord`, `pair::tests`, and
//!   the shipping KVM 1-vCPU and 8-vCPU `register-ipc` benchmarks.

#[cfg(not(test))]
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU64, Ordering};

use kernel_object::api::identity::ObjectIdentity;
use nucleus_core::util::cache_padded::CachePadded;
#[cfg(test)]
use nucleus_core::util::lockdep::LockClass;
use nucleus_core::util::lockdep::{DispatchPreemption, MAX_TRACKED_CPUS};

use crate::multitask::cpu_local;
use crate::multitask::current_identity::execution as identity_execution;
#[cfg(test)]
use crate::multitask::irq_leaf::IrqLeaf;

use super::context_validation::PublishedFrameView;
use super::return_architecture::ArchitectureState;
use super::runqueue::{
    self, RunOwnerSnapshot, RunOwnerState,
    affinity_payload::{self, PlacementGrant},
    wait::{REASON_ENDPOINT_RECEIVE as RECEIVE, REASON_ENDPOINT_REPLY as REPLY, REASON_NONE},
};
use super::stack_resources::{self, StackPin};
use super::{
    SCHED_MIN_GRANULARITY_NS, dispatch_policy, donation_ledger, donation_publication,
    execution_interval, frame_publication, ipc_lane, ipc_publication, local_dispatch, resident,
    runtime_profile, sync_handoff, task_directory, task_weight,
};

const PHASE_MASK: u64 = 0x3;
const PHASE_VACANT: u64 = 0;
const PHASE_PREPARING: u64 = 1;
const PHASE_LIVE: u64 = 2;
const PHASE_BREAKING: u64 = 3;

const FLAG_BREAK_REQUESTED: u64 = 1 << 2;
const FLAG_WAKE_PARTNER: u64 = 1 << 3;
const GEN_SHIFT: u32 = 16;

struct PairControl {
    state: AtomicU64,
    partner_slot_plus1: AtomicU64,
    partner_owner_gen: AtomicU64,
    generation_counter: AtomicU64,
}

impl PairControl {
    const fn new() -> Self {
        Self {
            state: AtomicU64::new(PHASE_VACANT),
            partner_slot_plus1: AtomicU64::new(0),
            partner_owner_gen: AtomicU64::new(0),
            generation_counter: AtomicU64::new(1),
        }
    }

    #[inline]
    fn pack(phase: u64, flags: u64, pair_gen: u64) -> u64 {
        (pair_gen << GEN_SHIFT) | (flags & !PHASE_MASK & ((1 << GEN_SHIFT) - 1)) | (phase & PHASE_MASK)
    }
}

static PAIR_CONTROLS: [CachePadded<PairControl>; MAX_TRACKED_CPUS] =
    [const { CachePadded::new(PairControl::new()) }; MAX_TRACKED_CPUS];

#[derive(Clone, Copy)]
struct PairedMember {
    slot: usize,
    id: u64,
    owner_generation: u64,
    view: PublishedFrameView,
    arch: ArchitectureState,
    saved_rsp: usize,
    wait_reason: (u8, u64),
}

struct PairImage {
    pair_gen: u64,
    active_idx: usize,
    members: [Option<PairedMember>; 2],
    partner_pin: Option<StackPin>,
    partner_permit: Option<identity_execution::ExecutionPermit>,
    partner_placement: Option<PlacementGrant>,
}

#[cfg(test)]
// SAFETY: under host tests, `PAIR_IMAGES` is protected by `IrqLeaf`, and
// `PlacementGrant`'s `!Send` marker only prevents migration across CPUs in real execution.
unsafe impl Send for PairImage {}

impl PairImage {
    const fn empty() -> Self {
        Self {
            pair_gen: 0,
            active_idx: 0,
            members: [None, None],
            partner_pin: None,
            partner_permit: None,
            partner_placement: None,
        }
    }

    fn clear_metadata(&mut self) {
        self.pair_gen = 0;
        self.active_idx = 0;
        self.members = [None, None];
    }
}

#[cfg(not(test))]
struct CpuPairCell(UnsafeCell<PairImage>);
#[cfg(not(test))]
// SAFETY: accessed only by `cpu` under an IRQ-masked `DispatchPreemption` scope.
unsafe impl Sync for CpuPairCell {}

#[cfg(not(test))]
static PAIR_IMAGES: [CachePadded<CpuPairCell>; MAX_TRACKED_CPUS] =
    [const { CachePadded::new(CpuPairCell(UnsafeCell::new(PairImage::empty()))) }; MAX_TRACKED_CPUS];

#[cfg(test)]
static PAIR_IMAGES: [CachePadded<IrqLeaf<PairImage, { LockClass::CpuExecutionInterval as u8 }>>;
    MAX_TRACKED_CPUS] =
    [const { CachePadded::new(IrqLeaf::new(PairImage::empty())) }; MAX_TRACKED_CPUS];

#[inline]
fn with_pair_image<R>(cpu: usize, f: impl FnOnce(&mut PairImage) -> R) -> R {
    #[cfg(not(test))]
    {
        // SAFETY: owner CPU exclusively owns `PAIR_IMAGES[cpu]` with IRQs masked.
        f(unsafe { &mut *PAIR_IMAGES[cpu].0.get() })
    }
    #[cfg(test)]
    {
        f(&mut PAIR_IMAGES[cpu].lock())
    }
}

#[inline]
fn current_phase(word: u64) -> u64 {
    word & PHASE_MASK
}

pub(in crate::multitask::scheduler) fn reset_before_publication() {
    for cpu in 0..MAX_TRACKED_CPUS {
        PAIR_CONTROLS[cpu].partner_slot_plus1.store(0, Ordering::Release);
        PAIR_CONTROLS[cpu].partner_owner_gen.store(0, Ordering::Release);
        PAIR_CONTROLS[cpu].state.store(PHASE_VACANT, Ordering::Release);
        with_pair_image(cpu, |img| {
            let _ = img.partner_pin.take();
            let _ = img.partner_permit.take();
            let _ = img.partner_placement.take();
            img.clear_metadata();
        });
    }
}

/// Lock-free conservation predicate: true when `cpu`'s pair descriptor currently
/// holds `slot` at `expected_gen` as its inactive `Paired` partner.
pub(in crate::multitask::scheduler) fn holds_partner(
    cpu: usize,
    slot: usize,
    expected_gen: u64,
) -> bool {
    let Some(ctrl) = PAIR_CONTROLS.get(cpu) else {
        return false;
    };
    let state = ctrl.state.load(Ordering::Acquire);
    let phase = current_phase(state);
    if !matches!(phase, PHASE_PREPARING | PHASE_LIVE | PHASE_BREAKING) {
        return false;
    }
    let partner = ctrl.partner_slot_plus1.load(Ordering::Acquire);
    let partner_gen = ctrl.partner_owner_gen.load(Ordering::Acquire);
    partner == (slot as u64) + 1 && partner_gen == expected_gen
}

#[inline]
pub(in crate::multitask) fn break_requested(cpu: usize) -> bool {
    PAIR_CONTROLS.get(cpu).is_some_and(|ctrl| {
        let word = ctrl.state.load(Ordering::Acquire);
        matches!(current_phase(word), PHASE_PREPARING | PHASE_LIVE)
            && (word & FLAG_BREAK_REQUESTED) != 0
    })
}

fn post_break_flags(cpu: usize, slot: usize, expected_gen: u64, add_flags: u64) {
    let Some(ctrl) = PAIR_CONTROLS.get(cpu) else {
        return;
    };
    let _ = ctrl.state.fetch_update(Ordering::AcqRel, Ordering::Acquire, |word| {
        let phase = current_phase(word);
        if !matches!(phase, PHASE_PREPARING | PHASE_LIVE) {
            return None;
        }
        let p = ctrl.partner_slot_plus1.load(Ordering::Acquire);
        let g = ctrl.partner_owner_gen.load(Ordering::Acquire);
        if p != (slot as u64) + 1 || g != expected_gen {
            return None;
        }
        Some(word | FLAG_BREAK_REQUESTED | add_flags)
    });
}

pub(in crate::multitask::scheduler) fn request_break_for_retire(
    cpu: usize,
    slot: usize,
    expected_gen: u64,
) {
    post_break_flags(cpu, slot, expected_gen, 0);
}

pub(in crate::multitask::scheduler) fn request_break_for_affinity(
    cpu: usize,
    slot: usize,
    expected_gen: u64,
) {
    post_break_flags(cpu, slot, expected_gen, 0);
}

pub(in crate::multitask::scheduler) fn request_break_with_wake(
    cpu: usize,
    slot: usize,
    expected_gen: u64,
) {
    post_break_flags(cpu, slot, expected_gen, FLAG_WAKE_PARTNER);
}

#[inline]
fn stack_rsp_in_view(rsp: usize, view: PublishedFrameView) -> bool {
    let addr = rsp as u64;
    view.kernel_stack_base != 0
        && addr >= view.kernel_stack_base
        && addr < view.kernel_stack_top
        && addr & 0xf == 0
}

#[inline]
fn cpu_eligible(slot: usize, cpu: usize) -> bool {
    let (task_mask, process_mask, migration_pending) = affinity_payload::affinity_snapshot(slot);
    !migration_pending && ((task_mask & process_mask) & (1_u64 << cpu)) != 0
}

/// Dissolves any live pair on the current CPU before ordinary scheduling, yield,
/// exit, or non-paired context switches. Must be called under `DispatchPreemption`
/// with IRQs masked on the owning CPU.
pub(in crate::multitask) fn dissolve_if_live(scope: &DispatchPreemption<'_>) {
    if !ipc_publication::scoped(scope) {
        return;
    }
    let cpu = scope.cpu();
    let ctrl = &PAIR_CONTROLS[cpu];
    let prev = ctrl.state.load(Ordering::Acquire);
    if current_phase(prev) != PHASE_LIVE {
        return;
    }
    // Atomically transition Live -> Breaking while capturing any concurrent wake/retire flag.
    let mut final_word = prev;
    let _ = ctrl.state.fetch_update(Ordering::AcqRel, Ordering::Acquire, |word| {
        if current_phase(word) != PHASE_LIVE {
            return None;
        }
        let next = (word & !PHASE_MASK) | PHASE_BREAKING;
        final_word = next;
        Some(next)
    });
    if current_phase(final_word) != PHASE_LIVE && current_phase(ctrl.state.load(Ordering::Acquire)) != PHASE_BREAKING {
        return;
    }

    let (partner, pin, permit, placement) = with_pair_image(cpu, |img| {
        let partner_idx = img.active_idx ^ 1;
        let partner = img.members[partner_idx].take();
        let pin = img.partner_pin.take();
        let permit = img.partner_permit.take();
        let placement = img.partner_placement.take();
        img.clear_metadata();
        (partner, pin, permit, placement)
    });

    if let Some(partner) = partner {
        let wake_partner = (ctrl.state.load(Ordering::Acquire) & FLAG_WAKE_PARTNER) != 0;
        frame_publication::publish(
            partner.slot,
            partner.saved_rsp,
            partner.view.kernel_stack_base,
            partner.view.kernel_stack_top,
            partner.view.alternate_stack_base,
            partner.view.alternate_stack_top,
            partner.view.user_mode_task,
            partner.view.kernel_process_task,
            true,
            partner.view.scheduler_storage_base,
            partner.view.scheduler_storage_end,
        );
        runqueue::set_saved_rsp(partner.slot, partner.saved_rsp);

        if wake_partner {
            let weight = runqueue::weight::value(partner.slot);
            if runqueue::pair_break_local(partner.slot, cpu, weight).is_some() {
                runqueue::wait::set_ready_since_ticks(partner.slot, crate::arch::rtc::ticks().max(1));
            }
        } else {
            let (kind, key) = partner.wait_reason;
            let expected = runqueue::owner(partner.slot);
            if expected.state == RunOwnerState::Paired && expected.cpu == Some(cpu) {
                let blocked = runqueue::wait::dissolve_paired_to_blocked(partner.slot, kind, key, || {
                    runqueue::pair_break_blocked(partner.slot, expected)
                        .expect("paired partner owner raced dissolve_paired_to_blocked")
                });
                frame_publication::bind_local_owner_generation(partner.slot, blocked.generation);
            }
        }
    }

    ctrl.partner_slot_plus1.store(0, Ordering::Release);
    ctrl.partner_owner_gen.store(0, Ordering::Release);
    ctrl.state.store(PHASE_VACANT, Ordering::Release);

    drop(placement);
    drop(permit);
    drop(pin);
}

#[inline]
fn record_pair_accounting_and_publish(
    cpu: usize,
    from: &PairedMember,
    to: &mut PairedMember,
    to_running: RunOwnerSnapshot,
    reply_label: u64,
    next_rsp: usize,
    prev_arch: ArchitectureState,
    clock: &crate::arch::rtc::ClockSample,
) {
    to.owner_generation = to_running.generation;
    let hop = execution_interval::forward(
        cpu,
        (from.slot, from.id),
        (to.slot, to.id, to_running.generation),
        reply_label,
        clock,
    );
    runtime_profile::owner::lane_account(cpu, from.slot, hop);
    runqueue::charge_cpu_time(from.slot, hop);
    runqueue::add_vruntime(
        from.slot,
        task_weight::weighted_vruntime_delta(
            hop.max(SCHED_MIN_GRANULARITY_NS),
            runqueue::weight::value(from.slot),
        ),
    );
    runqueue::wait::set_ready_since_ticks(to.slot, 0);
    dispatch_policy::record_completed_choice(
        cpu,
        !runqueue::is_idle_slot(to.slot) && !local_dispatch::is_user_class(to.slot),
        false,
    );
    sync_handoff::record_dispatch(cpu, true, clock.ticks);
    runtime_profile::owner::lane_dispatch(cpu, to.slot, prev_arch.root != to.arch.root);
    affinity_payload::record_last_cpu(to.slot, cpu as u8);
    cpu_local::set_scheduler_current_task_scratch_for_cpu(cpu, to.slot);
    runqueue::set_cpu_running_idle(cpu, runqueue::is_idle_slot(to.slot));
    super::return_architecture::lane_install(cpu, prev_arch, to.arch, to.slot, to.id);
    ipc_lane::note_installed(cpu, to.slot, to.arch);
    cpu_local::pair_publish(cpu, from.slot, to.slot);
    nucleus_core::util::lockdep::record_scheduler_dispatch(
        cpu, from.id, to.id, from.slot, to.slot, next_rsp, false, false,
    );
    nucleus_core::util::lockdep::set_current_task_owner(
        to.id.checked_add(1).expect("task lock owner exhausted"),
    );
}

/// Attempts a paired call switch (`C -> S`), either via an existing live pair
/// (`Warm`) or by forming a fresh pair on `cpu` (`Cold`). Returns `Some(next_rsp)`
/// on switch and `None` cleanly to let `ipc_lane::call` fall back if ineligible.
pub(in crate::multitask::scheduler) fn try_call(
    scope: &DispatchPreemption<'_>,
    endpoint: u64,
    reply: u64,
    receiver_id: u64,
    rsp: usize,
    clock: &crate::arch::rtc::ClockSample,
) -> Option<usize> {
    let cpu = scope.cpu();
    if !ipc_publication::direct_switch_admitted(cpu)
        || dispatch_policy::atomic_activation_pending(cpu)
        || sync_handoff::pending(cpu)
        || donation_ledger::class_publication_sequence() & 1 != 0
    {
        dissolve_if_live(scope);
        return None;
    }

    let c = cpu_local::current_cpu_task_slot()?;
    let c_id = stack_resources::task_start::read(c).map(|s| s.id)?;
    let c_owner = runqueue::owner(c);
    if c_id == receiver_id
        || runqueue::is_idle_slot(c)
        || c == super::ROOT_TASK_SLOT
        || c_owner.state != RunOwnerState::Running
        || c_owner.cpu != Some(cpu)
        || !c_owner.runnable
        || !c_owner.wait_armed
        || c_owner.wait_reason_kind != REPLY
        || !stack_resources::cpu_leases::owns_current(cpu, c)
        || !identity_execution::banks::owns_current(cpu, c)
        || runqueue::wait::reason_snapshot(c) != (REPLY, reply)
    {
        return None;
    }

    let ctrl = &PAIR_CONTROLS[cpu];
    let phase = current_phase(ctrl.state.load(Ordering::Acquire));

    if phase == PHASE_LIVE {
        if let Some(next_rsp) = try_warm_call(
            scope, cpu, c, c_id, c_owner, endpoint, reply, receiver_id, rsp, clock,
        ) {
            return Some(next_rsp);
        }
        // Any mismatched, stale, or break-requested pair is dissolved before
        // falling back to ordinary `ipc_lane::call` or cold pair formation.
        dissolve_if_live(scope);
    } else if phase != PHASE_VACANT {
        return None;
    }

    try_cold_call(scope, cpu, c, c_id, c_owner, endpoint, reply, receiver_id, rsp, clock)
}

#[allow(clippy::too_many_arguments)]
fn try_warm_call(
    scope: &DispatchPreemption<'_>,
    cpu: usize,
    c: usize,
    c_id: u64,
    c_owner: RunOwnerSnapshot,
    endpoint: u64,
    reply: u64,
    receiver_id: u64,
    rsp: usize,
    clock: &crate::arch::rtc::ClockSample,
) -> Option<usize> {
    if break_requested(cpu) {
        return None;
    }
    let ctrl = &PAIR_CONTROLS[cpu];

    let (
        mut active_member,
        mut partner_member,
        partner_pin,
        partner_permit,
        partner_placement,
    ) = with_pair_image(cpu, |img| {
        let act_idx = img.active_idx;
        let part_idx = act_idx ^ 1;
        let (Some(act), Some(part)) = (img.members[act_idx], img.members[part_idx]) else {
            return None;
        };
        if act.slot != c
            || act.id != c_id
            || part.id != receiver_id
            || part.wait_reason != (RECEIVE, endpoint)
            || act.arch.root == 0
            || act.arch.root != part.arch.root
        {
            return None;
        }
        if !stack_rsp_in_view(rsp, act.view) {
            return None;
        }
        if !cpu_eligible(c, cpu) || !cpu_eligible(part.slot, cpu) {
            return None;
        }
        let s_owner = runqueue::owner(part.slot);
        if s_owner.state != RunOwnerState::Paired
            || s_owner.cpu != Some(cpu)
            || s_owner.generation != part.owner_generation
        {
            return None;
        }
        if !execution_interval::can_forward(cpu, c, c_id, c_id) {
            return None;
        }
        let donated = match donation_publication::bind_lane_current_paired(
            scope,
            reply,
            (c, c_id),
            part.slot,
            part.id,
        ) {
            Some(true) => true,
            _ => false,
        };
        if !donated || break_requested(cpu) {
            return None;
        }
        Some((
            act,
            part,
            img.partner_pin.take()?,
            img.partner_permit.take()?,
            img.partner_placement.take()?,
        ))
    })?;

    // Check c_owner can park and partner can resume.
    // SAFETY: masked lane scope; `c` is Current on `cpu`.
    unsafe { runqueue::simd_tls::save_simd_state_lane(c, c_id) };
    let Ok(c_paired) = runqueue::pair_park(c, c_owner, cpu) else {
        with_pair_image(cpu, |img| {
            img.partner_pin = Some(partner_pin);
            img.partner_permit = Some(partner_permit);
            img.partner_placement = Some(partner_placement);
        });
        return None;
    };

    let s_owner = runqueue::owner(partner_member.slot);
    let s_running = runqueue::pair_resume(partner_member.slot, s_owner, cpu)
        .expect("warm paired partner vanished during resume");

    let mut s_placement = partner_placement;
    s_placement.rebind_paired(s_running);

    let out_pin = stack_resources::cpu_leases::swap_paired(cpu, c, partner_pin);
    let out_permit = identity_execution::banks::swap_paired(cpu, c, partner_permit);
    let out_placement = cpu_local::dispatch_turn::swap_installed_placement(cpu, Some(s_placement));

    let next_rsp = partner_member.saved_rsp;
    active_member.owner_generation = c_paired.generation;
    active_member.saved_rsp = rsp;
    active_member.wait_reason = (REPLY, reply);

    let prev_arch = active_member.arch;
    record_pair_accounting_and_publish(
        cpu,
        &active_member,
        &mut partner_member,
        s_running,
        reply,
        next_rsp,
        prev_arch,
        clock,
    );

    with_pair_image(cpu, |img| {
        let act_idx = img.active_idx;
        let part_idx = act_idx ^ 1;
        img.members[act_idx] = Some(active_member);
        img.members[part_idx] = Some(partner_member);
        img.active_idx = part_idx;
        img.partner_pin = Some(out_pin);
        img.partner_permit = Some(out_permit);
        img.partner_placement = out_placement;
    });
    ctrl.partner_slot_plus1.store(c as u64 + 1, Ordering::Release);
    ctrl.partner_owner_gen.store(c_paired.generation, Ordering::Release);

    Some(next_rsp)
}

#[allow(clippy::too_many_arguments)]
fn try_cold_call(
    scope: &DispatchPreemption<'_>,
    cpu: usize,
    c: usize,
    c_id: u64,
    c_owner: RunOwnerSnapshot,
    endpoint: u64,
    reply: u64,
    receiver_id: u64,
    rsp: usize,
    clock: &crate::arch::rtc::ClockSample,
) -> Option<usize> {
    let ctrl = &PAIR_CONTROLS[cpu];
    if current_phase(ctrl.state.load(Ordering::Acquire)) != PHASE_VACANT {
        return None;
    }

    let c_view = frame_publication::return_view(c)?;
    if !c_view.user_mode_task || !stack_rsp_in_view(rsp, c_view) {
        return None;
    }
    let c_arch = stack_resources::context::retained_architecture(cpu, c, c_id, c_view)?;
    if !c_arch.process || c_arch.root == 0 {
        return None;
    }
    let c_root = c_arch.root;

    let s = task_directory::hint(receiver_id)
        .filter(|&s| s != c && !runqueue::is_idle_slot(s) && s != super::ROOT_TASK_SLOT)?;
    if stack_resources::task_start::read(s).map(|t| t.id) != Some(receiver_id) {
        return None;
    }
    let s_owner = runqueue::owner(s);
    if s_owner.state != RunOwnerState::Blocked
        || s_owner.wait_armed
        || s_owner.wait_reason_kind != RECEIVE
        || s_owner.cpu.is_some_and(|owner_cpu| owner_cpu != cpu)
        || runqueue::wait::reason_snapshot(s) != (RECEIVE, endpoint)
    {
        return None;
    }
    if runqueue::address_space::root(s) != c_root
        || !cpu_eligible(c, cpu)
        || !cpu_eligible(s, cpu)
        || !identity_execution::qualified_dispatchable(c, c_id)
        || !identity_execution::qualified_dispatchable(s, receiver_id)
        || !execution_interval::can_forward(cpu, c, c_id, c_id)
        || !cpu_local::dispatch_turn::has_installed_placement_for_paired(
            cpu,
            c,
            c_id,
            c_owner.generation,
        )
        || resident::parked_cpu(s, s_owner.generation) != Some(cpu)
    {
        return None;
    }
    let s_view = frame_publication::return_view(s).filter(|v| v.user_mode_task)?;
    if current_phase(ctrl.state.load(Ordering::Acquire)) != PHASE_VACANT {
        return None;
    }

    let donation_ok = matches!(
        donation_publication::bind_lane_current_paired(scope, reply, (c, c_id), s, receiver_id),
        Some(true)
    );
    if !donation_ok {
        return None;
    }

    let pair_gen = ctrl.generation_counter.fetch_add(1, Ordering::Relaxed);
    let preparing_word = PairControl::pack(PHASE_PREPARING, 0, pair_gen);
    if ctrl
        .state
        .compare_exchange(PHASE_VACANT, preparing_word, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return None;
    }

    // Park caller `c` into Paired (`Running -> Paired`) while masked on `cpu`.
    // `partner_slot_plus1` stays 0 until `lane_claim` succeeds, so any failure before
    // commit can resume `c` synchronously on `cpu` without leaking `s`.
    // SAFETY: masked lane scope; `c` is Current on `cpu`.
    unsafe { runqueue::simd_tls::save_simd_state_lane(c, c_id) };
    let Ok(c_paired) = runqueue::pair_park(c, c_owner, cpu) else {
        ctrl.state.store(PHASE_VACANT, Ordering::Release);
        return None;
    };

    let claimed = affinity_payload::with_placement_admission(s, Some(receiver_id), cpu, || {
        runqueue::wait::lane_wake_clear(s, RECEIVE, endpoint, || {
            runqueue::lane_claim(s, s_owner, cpu).ok()
        })
    })
    .and_then(|(claimed, grant)| claimed.map(|claimed| (claimed, grant)));
    let Some((claimed, mut s_placement)) = claimed else {
        let _ = runqueue::pair_resume(c, c_paired, cpu)
            .expect("paired caller vanished during cold claim rollback");
        ctrl.state.store(PHASE_VACANT, Ordering::Release);
        return None;
    };

    let s_running = claimed.running();
    let loans = claimed
        .take_resident()
        .expect("cold paired claim won owner CAS after parked_cpu match without resident loans");
    s_placement.bind_claimed(s_running);

    ctrl.partner_slot_plus1.store(c as u64 + 1, Ordering::Release);
    ctrl.partner_owner_gen.store(c_paired.generation, Ordering::Release);

    let c_pin = stack_resources::cpu_leases::swap_paired(cpu, c, loans.pin);
    let c_permit = identity_execution::banks::swap_paired(cpu, c, loans.permit);
    let c_placement = cpu_local::dispatch_turn::swap_installed_placement(cpu, Some(s_placement));

    let mut caller_member = PairedMember {
        slot: c,
        id: c_id,
        owner_generation: c_paired.generation,
        view: c_view,
        arch: c_arch,
        saved_rsp: rsp,
        wait_reason: (REPLY, reply),
    };
    let mut server_member = PairedMember {
        slot: s,
        id: receiver_id,
        owner_generation: s_running.generation,
        view: s_view,
        arch: loans.arch,
        saved_rsp: loans.saved_rsp,
        wait_reason: (REASON_NONE, 0),
    };

    let next_rsp = loans.saved_rsp;
    record_pair_accounting_and_publish(
        cpu,
        &caller_member,
        &mut server_member,
        s_running,
        reply,
        next_rsp,
        c_arch,
        clock,
    );

    let _ = &mut caller_member;
    with_pair_image(cpu, |img| {
        img.pair_gen = pair_gen;
        img.active_idx = 1;
        img.members = [Some(caller_member), Some(server_member)];
        img.partner_pin = Some(c_pin);
        img.partner_permit = Some(c_permit);
        img.partner_placement = c_placement;
    });
    let _ = ctrl.state.fetch_update(Ordering::AcqRel, Ordering::Acquire, |word| {
        Some((word & !PHASE_MASK) | PHASE_LIVE)
    });

    Some(next_rsp)
}

/// Attempts a warm paired reply switch (`S -> C`). Returns `Some(next_rsp)` on hit,
/// or dissolves any invalid/break-requested live pair cleanly and returns `None` so
/// `ipc_lane::reply` falls back to `ResidentCell` / generic wakeup.
pub(in crate::multitask::scheduler) fn try_reply(
    scope: &DispatchPreemption<'_>,
    reply: u64,
    caller_id: u64,
    context_owner: u64,
    context: ObjectIdentity,
    rsp: usize,
    clock: &crate::arch::rtc::ClockSample,
) -> Option<usize> {
    let cpu = scope.cpu();
    let ctrl = &PAIR_CONTROLS[cpu];
    if current_phase(ctrl.state.load(Ordering::Acquire)) != PHASE_LIVE {
        return None;
    }
    if break_requested(cpu)
        || !ipc_publication::direct_switch_admitted(cpu)
        || dispatch_policy::atomic_activation_pending(cpu)
        || sync_handoff::pending(cpu)
        || donation_ledger::class_publication_sequence() & 1 != 0
    {
        dissolve_if_live(scope);
        return None;
    }

    let s = cpu_local::current_cpu_task_slot()?;
    let s_id = stack_resources::task_start::read(s).map(|t| t.id)?;
    let s_owner = runqueue::owner(s);
    if s_id == caller_id
        || runqueue::is_idle_slot(s)
        || s == super::ROOT_TASK_SLOT
        || s_owner.state != RunOwnerState::Running
        || s_owner.cpu != Some(cpu)
        || s_owner.runnable
        || s_owner.wait_armed
        || s_owner.wait_reason_kind != RECEIVE
        || !stack_resources::cpu_leases::owns_current(cpu, s)
        || !identity_execution::banks::owns_current(cpu, s)
    {
        dissolve_if_live(scope);
        return None;
    }
    let (exact, live) = task_directory::reply_context_custody(context_owner, context);
    if !exact && live {
        dissolve_if_live(scope);
        return None;
    }
    let endpoint = runqueue::wait::reason_snapshot(s).1;
    if endpoint == 0 {
        dissolve_if_live(scope);
        return None;
    }

    let warm = with_pair_image(cpu, |img| {
        let act_idx = img.active_idx;
        let part_idx = act_idx ^ 1;
        let (Some(act), Some(part)) = (img.members[act_idx], img.members[part_idx]) else {
            return None;
        };
        if act.slot != s
            || act.id != s_id
            || part.id != caller_id
            || part.wait_reason != (REPLY, reply)
            || act.arch.root == 0
            || act.arch.root != part.arch.root
        {
            return None;
        }
        if !stack_rsp_in_view(rsp, act.view)
            || !cpu_eligible(s, cpu)
            || !cpu_eligible(part.slot, cpu)
        {
            return None;
        }
        let c_owner = runqueue::owner(part.slot);
        if c_owner.state != RunOwnerState::Paired
            || c_owner.cpu != Some(cpu)
            || c_owner.generation != part.owner_generation
        {
            return None;
        }
        if !execution_interval::can_forward(cpu, s, s_id, part.id) {
            return None;
        }
        Some((
            act,
            part,
            img.partner_pin.take()?,
            img.partner_permit.take()?,
            img.partner_placement.take()?,
        ))
    });

    let Some((mut server_member, mut caller_member, c_pin, c_permit, mut c_placement)) = warm else {
        dissolve_if_live(scope);
        return None;
    };

    let _ = donation_ledger::release_reply_lane(caller_member.slot, caller_id, reply);
    // SAFETY: masked lane scope; `s` is Current on `cpu`.
    unsafe { runqueue::simd_tls::save_simd_state_lane(s, s_id) };
    let Ok(s_paired) = runqueue::pair_park(s, s_owner, cpu) else {
        with_pair_image(cpu, |img| {
            img.partner_pin = Some(c_pin);
            img.partner_permit = Some(c_permit);
            img.partner_placement = Some(c_placement);
        });
        dissolve_if_live(scope);
        return None;
    };

    let c_owner = runqueue::owner(caller_member.slot);
    let c_running = runqueue::pair_resume(caller_member.slot, c_owner, cpu)
        .expect("warm paired caller vanished during resume");
    c_placement.rebind_paired(c_running);

    let out_pin = stack_resources::cpu_leases::swap_paired(cpu, s, c_pin);
    let out_permit = identity_execution::banks::swap_paired(cpu, s, c_permit);
    let out_placement = cpu_local::dispatch_turn::swap_installed_placement(cpu, Some(c_placement));

    let next_rsp = caller_member.saved_rsp;
    server_member.owner_generation = s_paired.generation;
    server_member.saved_rsp = rsp;
    server_member.wait_reason = (RECEIVE, endpoint);

    let prev_arch = server_member.arch;
    record_pair_accounting_and_publish(
        cpu,
        &server_member,
        &mut caller_member,
        c_running,
        0,
        next_rsp,
        prev_arch,
        clock,
    );

    with_pair_image(cpu, |img| {
        let act_idx = img.active_idx;
        let part_idx = act_idx ^ 1;
        img.members[act_idx] = Some(server_member);
        img.members[part_idx] = Some(caller_member);
        img.active_idx = part_idx;
        img.partner_pin = Some(out_pin);
        img.partner_permit = Some(out_permit);
        img.partner_placement = out_placement;
    });
    ctrl.partner_slot_plus1.store(s as u64 + 1, Ordering::Release);
    ctrl.partner_owner_gen.store(s_paired.generation, Ordering::Release);

    Some(next_rsp)
}

#[cfg(test)]
mod tests;
