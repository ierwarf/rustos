//! Owner: actual root-born exec image producer and process-state join witnesses.
//! Boundary: real FrameWriter/backing/MM owners; native Current/HAL namespace is modeled.
//! Lifecycle: real root -> intent/census -> stage -> shared image -> finalize -> cleanup.
//! Concurrency: same-CPU scope, frozen members and exact staged MM retained through ACK.
//! Failure: foreign tuple refuses before image effects; native cleanup is explicit external ACK.
//! Forbidden: native catalog as physical publication or modeled architecture as hardware proof.
//! Evidence: shared all-build engine, actual frame bytes/metadata/MM/affinity and consumed-self bounds.
use super::*;
use crate::multitask::{
    process_placement::exec_membership::ExecMembership, process_table as process,
};
const ID: u64 = 81300;
const ROOT: u64 = 0x9000;
fn boot(mask: u64, fs: u64) -> UserTaskBootstrap {
    let mut b = super::super::super::root_creation::tests::bootstrap(UserAbi::Linux);
    b.entry = VirtAddr::new(crate::memory::paging::USER_SPACE_BASE + 0x440000);
    b.stack_pointer = VirtAddr::new(crate::memory::paging::USER_SPACE_BASE + 0x780000);
    b.linux_thread_state = Some(LinuxThreadState {
        signal_mask: mask,
        fs_base: fs,
        ..LinuxThreadState::default()
    });
    b
}
fn image(
    current: bool,
    f: impl FnOnce(
        &ExecImageLease<'_>,
        &DispatchPreemption,
        &mut UserTaskBootstrap,
    ) -> Option<ExecPublication>,
) {
    super::super::super::root_creation::tests::exec_root(
        ID,
        current,
        boot(0x55, 0),
        |slot, handle| {
            let reservation = process::begin_exec(handle).unwrap();
            let intent = identity::exec_intent::begin(slot, ID, reservation, current).unwrap();
            let census = ExecMembership::begin(&intent).unwrap();
            assert!(process::authorize_exec(reservation));
            let finalized = super::super::with_census_image(
                &intent,
                &census,
                reservation,
                ROOT,
                current,
                |lease| {
                    let staged = process::stage_exec_state(
                        reservation,
                        ProcessAddressSpace::modeled_root_for_host(ROOT),
                        crate::user::linux::LinuxProcessState::default(),
                        crate::user::linux::LinuxMemoryMapState::new(),
                        crate::user::linux::LinuxRuntimeProfile::new(),
                        "/owned-exec.elf",
                    )
                    .unwrap();
                    census.mark_staged();
                    lease.bind_staged_mm(&staged);
                    let mut incoming = boot(0xee, 0x8800);
                    incoming.registers.rax = 0x7711;
                    incoming
                        .linux_thread_state
                        .as_mut()
                        .unwrap()
                        .pending_signals = 0xff;
                    incoming
                        .linux_thread_state
                        .as_mut()
                        .unwrap()
                        .pending_sigchld_events = 0xff;
                    let preemption = DispatchPreemption::enter_model(0);
                    let published = cpu_local::with_exec_image_turn(preemption, lease, |turn| {
                        f(lease, turn, &mut incoming).unwrap_or_else(|| {
                            commit(lease, turn, &mut incoming, Some((ID, ID))).unwrap()
                        })
                    })
                    .unwrap();
                    Some(process::finalize_exec_state(staged, published))
                },
            )
            .unwrap()
            .unwrap();
            census.finish(&finalized);
            let (_, _, _, old) = finalized.into_parts();
            old.retire_execution_state();
            assert!(!identity::exec_intent::busy(slot));
            assert_eq!(
                stack_resources::context::read(slot).unwrap().process_handle,
                Some(handle)
            );
            assert_eq!(runqueue::address_space::root(slot), ROOT);
        },
    );
}
#[test]
pub(in crate::multitask::scheduler) fn actual_peer_exec_publishes_owned_frame_metadata_tls_and_staged_mm_before_finalization() {
    image(false, |lease, turn, b| {
        let slot = lease.slot;
        let before = runqueue::affinity_payload::affinity_snapshot(slot);
        let context = stack_resources::context::read(slot).unwrap();
        let budget = context.scheduling_context.identity();
        let old_rsp = runqueue::saved_rsp(slot);
        let old_epoch = frame_publication::test_read(slot).unwrap().frame_epoch;
        let publication = commit(lease, turn, b, Some((ID, ID))).unwrap();
        publication.assert_matches(lease.reservation, ROOT);
        assert_eq!(runqueue::address_space::root(slot), ROOT);
        assert_eq!(identity::read(slot).unwrap().abi, Some(UserAbi::Linux));
        assert_eq!(runqueue::affinity_payload::affinity_snapshot(slot), before);
        assert_eq!(
            stack_resources::context::read(slot)
                .unwrap()
                .scheduling_context
                .identity(),
            budget
        );
        assert_eq!(runqueue::saved_rsp(slot), old_rsp);
        let frame = frame_publication::test_read(slot).unwrap();
        assert!(frame.frame_epoch > old_epoch);
        // SAFETY: exact initialized writer geometry and image loan remain live.
        let saved = unsafe { context_validation::copy_saved_context(frame.saved_rsp) }.unwrap();
        assert_eq!(saved.rip, b.entry.as_u64());
        assert_eq!(saved.rsp, b.stack_pointer.as_u64());
        assert_eq!(saved.rax, 0x7711);
        assert_eq!(b.linux_thread_state.unwrap().signal_mask, 0x55);
        assert_eq!(b.linux_thread_state.unwrap().pending_signals, 0);
        assert_eq!(b.linux_thread_state.unwrap().pending_sigchld_events, 0);
        assert_eq!(runqueue::simd_tls::tls_fs_base(slot), 0x8800);
        let state = linux_thread_state::test_exec_state(slot, ID).unwrap();
        assert_eq!(
            (
                state.signal_mask,
                state.pending_signals,
                state.pending_sigchld_events
            ),
            (0x55, 0, 0)
        );
        assert!(
            stack_resources::context::lease_execution_root(slot, ROOT, turn.cpu()).is_none(),
            "exec image admitted a new execution root before image release"
        );
        // The common driver finalizes using this exact once-only publication.
        // Its second commit must refuse, while retaining the original receipt.
        assert!(commit(lease, turn, b, Some((ID, ID))).is_none());
        Some(publication)
    });
}
#[test]
fn actual_peer_exec_foreign_pid_tid_and_mode_refuse_before_original_bytes_and_authority() {
    image(false, |lease, turn, b| {
        let rsp = runqueue::saved_rsp(lease.slot);
        let root = runqueue::address_space::root(lease.slot);
        let before = identity::read(lease.slot).unwrap();
        assert!(commit(lease, turn, b, None).is_none());
        assert!(commit(lease, turn, b, Some((ID + 1, ID))).is_none());
        assert!(commit(lease, turn, b, Some((ID, ID + 1))).is_none());
        assert_eq!(runqueue::saved_rsp(lease.slot), rsp);
        assert_eq!(runqueue::address_space::root(lease.slot), root);
        assert_eq!(identity::read(lease.slot).unwrap(), before);
        assert!(!lease.published.get());
        None
    });
}

#[test]
fn actual_exec_copied_current_without_original_bank_refuses_before_frame_and_mm_effects() {
    image(false, |lease, turn, b| {
        let before = runqueue::address_space::root(lease.slot);
        let copied = cpu_local::install_test_current_owner(0, 120);
        assert!(commit(lease, turn, b, Some((ID, ID))).is_none());
        assert_eq!(runqueue::address_space::root(lease.slot), before);
        assert!(!lease.published.get());
        drop(copied);
        None
    });
}
#[test]
pub(in crate::multitask::scheduler) fn actual_self_exec_never_rewrites_consumed_frame_or_publishes_ready_and_retains_original_bank() {
    image(true, |lease, turn, b| {
        let rsp = runqueue::saved_rsp(lease.slot);
        // SAFETY: native modeled Current holds actual sole bank and image writer;
        // this snapshot is a witness only, never consumed-frame execution authority.
        let before = unsafe { context_validation::copy_saved_context(rsp) }.unwrap();
        let owner = runqueue::owner(lease.slot);
        let affinity = runqueue::affinity_payload::affinity_snapshot(lease.slot);
        let publication = commit(lease, turn, b, None).unwrap();
        publication.assert_matches(lease.reservation, ROOT);
        // SAFETY: the same initialized native bank/writer still retain these
        // modeled bytes; this test does not grant a product consumed-frame read.
        let after = unsafe { context_validation::copy_saved_context(rsp) }.unwrap();
        assert_eq!(
            (after.rip, after.rsp, after.rax),
            (before.rip, before.rsp, before.rax)
        );
        assert_eq!(runqueue::owner(lease.slot), owner);
        assert_eq!(runqueue::affinity_payload::affinity_snapshot(lease.slot), affinity);
        assert_eq!(runqueue::saved_rsp(lease.slot), rsp);
        assert_eq!(runqueue::wait::ready_since_ticks(lease.slot), 0);
        assert!(stack_resources::cpu_leases::owns_current(
            turn.cpu(),
            lease.slot
        ));
        assert!(commit(lease, turn, b, None).is_none());
        Some(publication)
    });
}
