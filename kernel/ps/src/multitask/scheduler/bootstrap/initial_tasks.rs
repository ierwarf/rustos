//! Owner: one-shot image-qualified BSP/AP kernel materialization.
//! Boundary: original InitialCpu + exact Ticket, never a Scheduler directory.
//! Lifecycle: hidden descriptor -> Blocked ACK -> initial Running/interval -> banks.
//! Concurrency: parked AP stack belongs to this original image until publication;
//! the producer backing loan overlaps both physical/UID Current banks.
//! Failure: partial image effects poison; no reset/retry of an admitted allocation.
//! Forbidden: copied legacy reset, scalar UID as backing or an AP local-clock start.
//! Evidence: shared kernel descriptor commit; native earlier HAL inputs are models.
use super::super::{
    NICE_0_LOAD, PublishedFrameView, STACK_CANARY_WORD, TASK_STACK_GUARD_BYTES, TASK_STACK_SIZE,
    align_kernel_stack_top, execution_image, frame_publication, kernel_creation, root_slots,
    stack_resources, task_weight,
};
use super::*;

impl InitialCpu<'_> {
    /// Shared physical materialization join. The original image remains borrowed
    /// through its last initial effect; the Ticket owns all descriptor writes.
    #[allow(clippy::too_many_arguments)]
    fn commit_kernel(
        &self,
        ticket: stack_resources::admission::Ticket,
        entry: fn(u64),
        weight: u32,
        root: u64,
        rsp: usize,
        view: PublishedFrameView,
    ) -> stack_resources::StackPin {
        self.check(EFFECTS);
        assert_eq!((ticket.slot(), ticket.task_id()), (self.slot(), self.id()));
        assert!(
            self.cpu() != 0 || rsp != 0,
            "BSP lacks materialized entry frame"
        );
        assert!(
            self.cpu() == 0 || rsp == 0,
            "AP entry must retain its live HAL RSP"
        );
        assert!(view.kernel_stack_top > view.kernel_stack_base);
        assert_eq!(view.kernel_stack_top & 0xf, 0);
        let now = crate::arch::rtc::ticks();
        let backing = kernel_creation::commit_image(
            ticket,
            entry,
            root,
            weight,
            0,
            now,
            rsp,
            view,
            1 << self.cpu(),
        );
        if self.cpu() != 0 {
            runqueue::set_idle_slot(self.slot(), true);
        }
        // Producer ACK left a complete hidden Blocked descriptor. No queue wake,
        // budget admission or peer selection substitutes for this initial image.
        runqueue::admit_initial(self);
        super::super::execution_interval::begin_bootstrap(self, now);
        self.check(EFFECTS);
        backing
    }
}

/// Initial image starts only from true empty resources, not teardown/reset of a
/// previous lifetime. This check precedes all global owner initialization.
pub(super) fn initialize_owners(image: &InitialCpu<'_>, pit: u16) {
    image.assert_effects();
    assert_eq!(image.cpu(), 0);
    stack_resources::assert_boot_vacant();
    runqueue::reset_before_publication();
    for slot in 0..MAX_TASK {
        crate::multitask::current_identity::clear(slot);
    }
    super::super::task_directory::reset();
    super::super::dispatch_policy::reset();
    super::super::sync_handoff::reset_all_cpus();
    super::super::donation_ledger::reset();
    super::super::runtime_profile::owner::reset_before_boot();
    super::super::pending_reply::reset();
    super::super::boot_execution::publish_tick(pit);
}

#[allow(clippy::too_many_arguments)]
pub(super) fn root(
    image: &InitialCpu<'_>,
    pit: u16,
    entry: fn(u64),
    cs: u64,
    ss: u64,
    flags: u64,
    rip: u64,
) {
    image.assert_effects();
    assert_eq!((image.cpu(), image.slot(), image.id()), (0, 0, 0));
    let ticket = root_slots::reserve(image.slot(), image.id())
        .expect("initial root producer allocation failed");
    let base = stack_resources::storage_base(image.slot());
    let rsp = execution_image::reserved_kernel_entry(&ticket, cs, ss, flags, rip, 0);
    let view = PublishedFrameView {
        kernel_stack_base: (base + TASK_STACK_GUARD_BYTES) as u64,
        kernel_stack_top: align_kernel_stack_top(base + TASK_STACK_SIZE) as u64,
        stack_canary_intact: stack_resources::owned_canary_intact(image.slot())
            .expect("initial root lost original guard"),
        user_mode_task: false,
        kernel_process_task: false,
        alternate_stack_base: 0,
        alternate_stack_top: 0,
        scheduler_storage_base: 0,
        scheduler_storage_end: 0,
    };
    let backing = image.commit_kernel(
        ticket,
        entry,
        task_weight::weight_from_pit_divisor(pit),
        crate::memory::paging::kernel_root_phys().as_u64(),
        rsp,
        view,
    );
    image.publish_current();
    // Only the original physical Current bank can authorize the first SIMD save.
    runqueue::simd_tls::save_simd_state(image.slot());
    drop(backing);
    nucleus_core::util::lockdep::set_current_task_owner(1);
}

pub(super) fn idle(image: &InitialCpu<'_>, entry: fn(u64)) {
    image.assert_effects();
    let (base, top) = image.ap_stack();
    assert!(
        top > base
            && top - base > (TASK_STACK_GUARD_BYTES + super::super::SAVED_CONTEXT_BYTES) as u64
    );
    let ticket = stack_resources::admission::reserve(image.slot(), image.id())
        .expect("initial AP producer is not vacant");
    // SAFETY: the original image proves this exact HAL CPU remains parked and
    // uniquely owns its static stack. Its active RSP is at the opposite end;
    // no saved AP frame is constructed or read on the BSP.
    for index in 0..TASK_STACK_GUARD_BYTES / core::mem::size_of::<u64>() {
        unsafe {
            core::ptr::write((base as *mut u64).add(index), STACK_CANARY_WORD);
        }
    }
    let view = PublishedFrameView {
        kernel_stack_base: base + TASK_STACK_GUARD_BYTES as u64,
        kernel_stack_top: top,
        stack_canary_intact: true,
        user_mode_task: false,
        kernel_process_task: false,
        alternate_stack_base: 0,
        alternate_stack_top: 0,
        scheduler_storage_base: 0,
        scheduler_storage_end: 0,
    };
    let backing = image.commit_kernel(
        ticket,
        entry,
        NICE_0_LOAD,
        crate::memory::paging::kernel_root_phys().as_u64(),
        0,
        view,
    );
    image.publish_current();
    drop(backing);
}

/// No frame access at AP entry: its first IRQ will publish the actual live RSP.
/// BSP validates its original saved image while the retained Current bank and
/// reader exclude rewrite/claim; architecture borrows the same bank afterwards.
pub(super) fn validate_root_frame(
    turn: &crate::multitask::cpu_local::dispatch_turn::CpuDispatchTurn<'static>,
) {
    let (cpu, slot) = turn.original_current().expect("boot lost original Current");
    assert_eq!((cpu, slot), (0, 0));
    let token = frame_publication::observed_frame_token(slot, cpu, true)
        .expect("initial root lost saved frame publication");
    let id = crate::multitask::current_identity::read_dispatch(slot)
        .and_then(|p| p.task_id)
        .expect("initial root lost UID");
    let pin = frame_publication::pin_for_identity(token, id)
        .expect("initial root lost physical frame custody");
    // SAFETY: original masked turn retains physical Current and initial image.
    let _reader = unsafe { stack_resources::frame_readers::acquire(&pin, cpu) }
        .expect("initial root frame is already consumed");
    super::super::context_validation::validate_published_saved_context(
        slot,
        token.saved_rsp,
        token.view,
    )
    .expect("initial root frame is invalid");
}

#[cfg(test)]
pub(super) mod tests;
