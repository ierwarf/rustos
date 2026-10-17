//! Owner: independent dynamic kernel construction and publication custody.
//! Boundary: actual Ticket, FrameWriter and pre-ACK physical child backing loan.
//! Lifecycle: complete hidden descriptor/Blocked ACK then qualified queue wake.
//! Concurrency: own-CPU IRQ/preemption; no directory or leaf across notification.
//! Failure: terminal overlap may refuse wake, never reuse the retained child UID.
//! Forbidden: native catalog, copied publication, or backing as dispatch permission.
//! Evidence: shared actual producer/queue/bank paths; bootstrap remains separate.
use super::*;

/// Observational initialization floor only, never a dispatch or funding grant.
/// Retain actual frame/UID backing while validating each eligible local candidate.
pub(super) fn creation_floor(cpu: usize) -> u64 {
    ready_floor(cpu, None).saturating_add(SCHED_NEW_TASK_VRUNTIME_PENALTY_NS)
}

/// Optional strict-band filter uses the same published launcher bit and live
/// inherited-system ledger as local dispatch. This is observation, not a grant.
pub(super) fn ready_floor(cpu: usize, system: Option<bool>) -> u64 {
    let mut floor: Option<u64> = None;
    let now = crate::arch::clock::monotonic_nanos();
    for slot in runqueue::local_runnable_slots(cpu) {
        if runqueue::is_idle_slot(slot) || (slot == ROOT_TASK_SLOT && cpu != 0) {
            continue;
        }
        if !runqueue::is_current_cpu_dispatchable(slot, cpu)
            || !runqueue::is_handoff_dispatchable(slot)
        {
            continue;
        }
        let candidate_system = runqueue::weight::value(slot) & SYSTEM_CLASS_WEIGHT_FLAG != 0
            || donation_ledger::inherited_system(slot);
        if system.is_some_and(|expected| expected != candidate_system) {
            continue;
        }
        if local_dispatch::published_candidate_admission(slot, cpu, now) != Ok(true) {
            continue;
        }
        let direct = runqueue::owner(slot).state == runqueue::RunOwnerState::DirectHandoff;
        let token = if direct {
            frame_publication::direct_handoff_frame_token(slot, cpu)
        } else {
            frame_publication::local_frame_token(slot, cpu)
        };
        let Some(token) = token else {
            continue;
        };
        let Some(id) = current_identity::read_dispatch(slot).and_then(|p| p.task_id) else {
            continue;
        };
        let Some(pin) = frame_publication::pin_for_identity(token, id) else {
            continue;
        };
        // SAFETY: the caller retains own-CPU IRQ exclusion. This real reader
        // and allocation pin exclude execution/image claims throughout validation.
        let Some(_reader) = (unsafe { stack_resources::frame_readers::acquire(&pin, cpu) }) else {
            continue;
        };
        if context_validation::validate_published_saved_context(slot, token.saved_rsp, token.view)
            .is_err()
        {
            continue;
        }
        let current = if direct {
            frame_publication::direct_handoff_token_is_current(token, cpu)
        } else {
            frame_publication::token_is_current(token, cpu)
        };
        if !current {
            continue;
        }
        let v = runqueue::vruntime(slot);
        floor = Some(floor.map_or(v, |old| old.min(v)));
    }
    floor.unwrap_or(0)
}
#[allow(clippy::too_many_arguments)]
pub(super) fn publish_reserved(
    ticket: stack_resources::admission::Ticket,
    entry: fn(u64),
    pit: u16,
    cs: u64,
    ss: u64,
    flags: u64,
    rip: u64,
    root: u64,
    vruntime: u64,
    before_wake: impl FnOnce(usize, u64),
    eligible_cpus: impl FnOnce(usize) -> u64,
) -> usize {
    let slot = ticket.slot();
    let id = ticket.task_id();
    assert!(
        ticket.is_reserved_current(),
        "kernel creator lost original producer"
    );
    assert_ne!(root, 0, "kernel creator lacks physical root namespace");
    let base = stack_resources::storage_base(slot);
    let kernel_base = (base + TASK_STACK_GUARD_BYTES) as u64;
    let kernel_top = align_kernel_stack_top(base + TASK_STACK_SIZE) as u64;
    let weight = task_weight::weight_from_pit_divisor(pit);
    let rsp = execution_image::reserved_kernel_entry(&ticket, cs, ss, flags, rip, 0);
    let view = PublishedFrameView {
        user_mode_task: false,
        kernel_process_task: false,
        kernel_stack_base: kernel_base,
        kernel_stack_top: kernel_top,
        alternate_stack_base: 0,
        alternate_stack_top: 0,
        stack_canary_intact: stack_resources::owned_canary_intact(slot)
            .expect("kernel producer lost guard"),
        scheduler_storage_base: 0,
        scheduler_storage_end: 0,
    };
    let backing = commit_image(ticket, entry, root, weight, vruntime, 0, rsp, view, u64::MAX);
    before_wake(slot, id);
    let eligible = eligible_cpus(slot);
    let target = runqueue_policy::affinity_target_cpu(slot, eligible);
    let cpu = nucleus_core::util::lockdep::current_cpu_index();
    let wake = if target == cpu {
        runqueue::publish_local_wake(slot, target, weight)
    } else {
        runqueue::publish_remote_wake(slot, target, weight)
    };
    // A committed kernel may be stopped concurrently after Slot ACK. Rejected
    // wake is not a constructor failure, retry on a new UID, or liveness proof.
    #[cfg(not(test))]
    if let runqueue::RemoteWakeOutcome::Published { cpu, notify: true } = wake {
        super::super::irq::request_target_reschedule(cpu);
    }
    #[cfg(test)]
    let _ = wake;
    drop(backing);
    slot
}

/// Sole hidden kernel descriptor commit, shared by dynamic and original image
/// producers. The exact Ticket owns construction and transfers its physical
/// backing BEFORE Blocked ACK. Neither the view nor this backing grants running.
#[allow(clippy::too_many_arguments)]
pub(super) fn commit_image(
    ticket: stack_resources::admission::Ticket,
    entry: fn(u64),
    root: u64,
    weight: u32,
    vruntime: u64,
    exec_start: u64,
    rsp: usize,
    view: PublishedFrameView,
    affinity: u64,
) -> stack_resources::StackPin {
    let slot = ticket.slot();
    let id = ticket.task_id();
    assert!(ticket.is_reserved_current(), "kernel image lost exact producer");
    assert_ne!(root, 0, "kernel image lacks root namespace");
    assert!(!view.user_mode_task && !view.kernel_process_task && view.stack_canary_intact);
    let kernel_base = view.kernel_stack_base;
    let kernel_top = view.kernel_stack_top;
    ticket.begin_commit();
    ticket.install_context(TaskContext {
        scheduling_context: scheduling_context::SchedulingContext::bind(slot, id),
        #[cfg(test)]
        saved_rsp: rsp,
        #[cfg(test)]
        test_ready: false,
        #[cfg(test)]
        ready_since_ticks: crate::arch::rtc::ticks(),
        #[cfg(test)]
        blocked: false,
        #[cfg(test)]
        blocked_since_ticks: 0,
        #[cfg(test)]
        wake_armed: false,
        #[cfg(test)]
        block_reason: BlockReason::None,
        #[cfg(test)]
        weight,
        #[cfg(test)]
        vruntime_ns: vruntime,
        #[cfg(test)]
        exec_start_ticks: exec_start,
        #[cfg(test)]
        address_space_root: root,
        #[cfg(test)]
        kernel_stack_base: kernel_base,
        #[cfg(test)]
        kernel_stack_top: kernel_top,
        #[cfg(test)]
        alternate_kernel_stack_base: 0,
        #[cfg(test)]
        alternate_kernel_stack_top: 0,
        user_mode: false,
        user_abi: None,
        console_session: ConsoleSessionHandle::SYSTEM,
        process_handle: None,
        process_id: None,
        user_stack: None,
        windows_thread_state: None,
    });
    runqueue::initialize_vruntime(slot, vruntime);
    runqueue::initialize_exec_start_ticks(slot, exec_start);
    runqueue::weight::initialize(slot, weight);
    runqueue::address_space::initialize(slot, root);
    runqueue::wait::initialize(slot);
    runqueue::wait::set_ready_since_ticks(slot, crate::arch::rtc::ticks());
    runqueue::initialize_saved_rsp(slot, rsp);
    runqueue::initialize_kernel_stack_bounds(slot, kernel_base, kernel_top);
    runqueue::initialize_alternate_kernel_stack_bounds(slot);
    runqueue::simd_tls::initialize_simd_state(slot, id);
    ticket.publish_start(TaskStart { entry, id });
    stack_resources::publish_binding(slot, id, view);
    frame_publication::publish(
        slot,
        rsp,
        kernel_base,
        kernel_top,
        0,
        0,
        false,
        false,
        view.stack_canary_intact,
        0,
        0,
    );
    task_directory::record(id, slot);
    current_identity::publish_metadata(
        slot,
        Some(TaskIdentity {
            task_id: Some(id),
            ..Default::default()
        }),
    );
    assert!(runqueue::affinity_payload::initialize_reserved(&ticket, affinity, affinity),
        "kernel constructor lost exact affinity publication");
    for gate in [
        current_identity::LifecycleGate::Retired,
        current_identity::LifecycleGate::Suspended,
        current_identity::LifecycleGate::JobStopped,
        current_identity::LifecycleGate::ExecQuiesced,
    ] {
        current_identity::update_live_lifecycle(slot, id, gate, false);
    }
    runqueue::admit_blocked(slot);
    frame_publication::bind_local_owner_generation(slot, runqueue::owner(slot).generation);
    // Capture physical custody BEFORE releasing the producer, not after a gap
    // in which another CPU can stop, reclaim and retag the same numerical slot.
    let backing = ticket.retain_publication(view);
    ticket.finish_commit();
    backing
}
#[allow(clippy::too_many_arguments)]
pub(in crate::multitask) fn allocate(
    entry: fn(u64),
    id: u64,
    pit: u16,
    cs: u64,
    ss: u64,
    flags: u64,
    rip: u64,
) -> Option<usize> {
    assert!(
        crate::multitask::cpu_local::scheduler_initialized(),
        "kernel creation preceded complete image"
    );
    let mut preemption = nucleus_core::util::lockdep::DispatchPreemption::enter();
    preemption.mask_irqs();
    let vruntime = creation_floor(preemption.cpu());
    for slot in FIRST_DYNAMIC_TASK_SLOT..MAX_TASK {
        let Some(ticket) = root_slots::reserve(slot, id) else {
            continue;
        };
        let slot = publish_reserved(
            ticket,
            entry,
            pit,
            cs,
            ss,
            flags,
            rip,
            crate::memory::paging::kernel_root_phys().as_u64(),
            vruntime,
            |_, _| {},
            runqueue_policy::online_affinity_mask,
        );
        super::super::irq::flush_deferred_target_reschedules();
        return Some(slot);
    }
    None
}
impl super::super::Thread {
    pub fn start(&self) {
        use x86_64::instructions::{
            interrupts,
            segmentation::{CS, SS, Segment},
        };
        interrupts::without_interrupts(|| {
            if self.slot.get().is_some() {
                return;
            }
            let cs = CS::get_reg().0 as u64;
            let ss = SS::get_reg().0 as u64;
            let flags = super::super::initial_task_rflags().bits();
            let rip = super::super::kernel_task_entry_trampoline_addr();
            let slot = allocate(self.entry, self.id, self.pit_divisor, cs, ss, flags, rip)
                .expect("No free task slot");
            self.slot.set(Some(slot));
        });
    }
}
#[cfg(test)]
pub(super) mod tests;
