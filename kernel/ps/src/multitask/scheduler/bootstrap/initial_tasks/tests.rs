//! Owner: native original image + born Ticket/descriptor/interval/bank witnesses.
//! Boundary: earlier HAL parked topology and CPU publication are explicit models.
//! Lifecycle: original materialization/Blocked ACK/Running interval through banks.
//! Concurrency: retained producer and Current loans deny reuse and duplicate entry.
//! Failure: foreign CPU, settled interval and live-resource reset refuse effects.
//! Forbidden: no privileged register installation, incoming RSP or static AP ACK.
//! Evidence: actual shared commit and architecture capture, not hardware return.
use super::super::super::{current_identity as identity, execution_interval, kernel_creation};
use super::*;
use crate::multitask::cpu_local;
use std::panic::{AssertUnwindSafe, catch_unwind};

pub(in crate::multitask::scheduler) fn born_initial(
    cpu: usize,
    f: impl FnOnce(&InitialCpu<'_>, stack_resources::StackPin),
) {
    let _fixture = super::super::super::tests::boxed_scheduler();
    let cell = ImageCell::new();
    let key = cell.claim(&[1, 2]).unwrap();
    let id = if cpu == 0 { 0 } else { 127011 };
    let image = key.cpu(cpu, id).unwrap();
    image.begin_effects();
    let ticket = root_slots::reserve(image.slot(), id).unwrap();
    let base = stack_resources::storage_base(image.slot());
    let rsp = if cpu == 0 {
        execution_image::reserved_kernel_entry(&ticket, 8, 16, 0x202, 0x1000, 0)
    } else {
        0
    };
    let view = PublishedFrameView {
        user_mode_task: false,
        kernel_process_task: false,
        kernel_stack_base: (base + TASK_STACK_GUARD_BYTES) as u64,
        kernel_stack_top: align_kernel_stack_top(base + TASK_STACK_SIZE) as u64,
        alternate_stack_base: 0,
        alternate_stack_top: 0,
        stack_canary_intact: stack_resources::owned_canary_intact(image.slot()).unwrap(),
        scheduler_storage_base: 0,
        scheduler_storage_end: 0,
    };
    let backing = image.commit_kernel(ticket, |_| {}, NICE_0_LOAD, 0x4000, rsp, view);
    f(&image, backing);
    let _ = execution_interval::consume(cpu, image.slot(), id);
    kernel_creation::tests::cleanup(image.slot());
    identity::clear(image.slot());
}

#[test]
fn actual_root_idle_requires_original_initial_current_banks_and_unconsumed_interval() {
    born_initial(0, |image, backing| {
        let banks = identity::execution::banks::isolate(0);
        let publication = cpu_local::install_test_current_owner(0, image.slot());
        stack_resources::cpu_leases::with_test_bank(0, backing, false, || {
            let scope = nucleus_core::util::lockdep::DispatchPreemption::enter_model(0);
            assert!(!super::super::super::current_observation::mark_root_idle_in(&scope));
            assert!(!runqueue::is_idle_slot(image.slot()));
            let permit = identity::execution::initial(image.slot(), image.id()).unwrap();
            identity::execution::banks::initialize(0, permit);
            assert!(super::super::super::current_observation::mark_root_idle_in(&scope));
            assert!(runqueue::is_idle_slot(image.slot()));
            let _ = execution_interval::consume(0, image.slot(), image.id());
            assert!(!super::super::super::current_observation::mark_root_idle_in(&scope));
        });
        drop(publication);
        drop(banks);
    });
}

#[test]
fn actual_initial_image_ack_joins_hidden_descriptor_running_interval_and_original_backing() {
    born_initial(0, |image, backing| {
        let slot = image.slot();
        let owner = runqueue::owner(slot);
        assert_eq!(owner.state, runqueue::RunOwnerState::Running);
        assert_eq!(owner.cpu, Some(0));
        assert!(!runqueue::wait::armed(slot) && !runqueue::wait::blocked(slot));
        assert!(!stack_resources::admission::is_reserved(slot));
        assert!(!runqueue::is_idle_slot(slot));
        assert_eq!(identity::read_dispatch(slot).unwrap().task_id, Some(0));
        assert_eq!(
            frame_publication::test_read(slot).unwrap().owner_generation,
            owner.generation
        );
        assert!(stack_resources::admission::reserve(slot, 1).is_none());
        assert!(catch_unwind(AssertUnwindSafe(|| stack_resources::context::clear(slot))).is_err());
        assert!(catch_unwind(AssertUnwindSafe(|| runqueue::admit_initial(image))).is_err());
        let execution_interval::ConsumedInterval::Active(interval) =
            execution_interval::consume(0, slot, 0)
        else {
            panic!("missing original boot interval");
        };
        assert!(interval.charge(100, 100).is_none());
        assert!(matches!(
            execution_interval::consume(0, slot, 0),
            execution_interval::ConsumedInterval::AlreadySettled
        ));
        drop(backing);
    });
}

#[test]
fn actual_ap_initial_image_owns_only_its_cpu_and_never_reads_a_synthetic_saved_rsp() {
    born_initial(1, |image, backing| {
        let slot = image.slot();
        assert_eq!(slot, MAX_TASK - 1);
        assert_eq!(runqueue::owner(slot).cpu, Some(1));
        assert!(runqueue::is_idle_slot(slot));
        assert_eq!(runqueue::saved_rsp(slot), 0);
        assert_eq!(
            runqueue::affinity_payload::affinity_snapshot(slot),
            (2, 2, false)
        );
        assert!(!runqueue::wait::armed(slot) && !runqueue::wait::blocked(slot));
        assert_eq!(runqueue::published_runnable_count(1), 0);
        let execution_interval::ConsumedInterval::Active(interval) =
            execution_interval::consume(1, slot, image.id())
        else {
            panic!("missing AP interval");
        };
        // No constructing-BSP local timestamp or funding/grant is transferred.
        assert!(interval.charge(100, 100).is_none());
        drop(backing);
    });
}

#[test]
fn actual_boot_architecture_borrows_original_image_current_and_interval_not_scalar_rsp() {
    born_initial(0, |image, backing| {
        let banks = identity::execution::banks::isolate(0);
        let permit = identity::execution::initial(image.slot(), image.id()).unwrap();
        identity::execution::banks::initialize(0, permit);
        let publication = cpu_local::install_test_current_owner(0, image.slot());
        image.image.cpus[0]
            .owner
            .store(runqueue::owner(image.slot()).generation, Ordering::Relaxed);
        image
            .begin_publication(runqueue::owner(image.slot()).generation)
            .unwrap();
        image.complete_publication();
        let hardware = HardwareScope {
            image: image.image,
            cpu: 0,
            phase: PUBLISHED,
        };
        stack_resources::cpu_leases::with_test_bank(0, backing, false, || {
            cpu_local::dispatch_turn::with_original_current_model(image.slot(), |turn| {
                validate_root_frame(turn);
                let plan = super::super::super::return_architecture::InitialArchitecture::capture(
                    &hardware, turn,
                )
                .unwrap();
                assert_eq!(
                    plan_state(&plan),
                    (0x4000, runqueue::kernel_stack_bounds(image.slot()).1)
                );
                drop(plan);
                let _ = execution_interval::consume(0, image.slot(), image.id());
                assert!(
                    super::super::super::return_architecture::InitialArchitecture::capture(
                        &hardware, turn
                    )
                    .is_none()
                );
            });
            assert!(stack_resources::admission::reserve(image.slot(), 1).is_none());
        });
        drop(publication);
        drop(banks);
    });
}

fn plan_state(
    plan: &super::super::super::return_architecture::InitialArchitecture<'_>,
) -> (u64, u64) {
    plan.test_state()
}

#[test]
fn actual_boot_owner_reset_refuses_existing_physical_lifetime_instead_of_tearing_it_down() {
    born_initial(0, |image, backing| {
        assert!(catch_unwind(AssertUnwindSafe(stack_resources::assert_boot_vacant)).is_err());
        assert_eq!(
            stack_resources::task_start::read(image.slot()).unwrap().id,
            image.id()
        );
        assert_eq!(
            runqueue::owner(image.slot()).state,
            runqueue::RunOwnerState::Running
        );
        drop(backing);
    });
}
