//! Owner: physical Current self-demotion regression witnesses, not catalog arrays.
//! Boundary: original modeled masked entry and real bank/binding/weight/ledger.
//! Lifecycle: born resources -> exact loan -> cap -> original bank drop -> cleanup ACK.
//! Concurrency: canonical native namespace; no copied slot grants or actual hardware claim.
//! Failure: scope, scalar-only Current, kernel context, sealed/foreign identity refuse.
//! Forbidden: demotion increasing weight or revoking reply custody.
//! Evidence: same production class engine; IRQ and cleanup ACK explicitly modeled.
use super::super::{
    donation_ledger, donation_publication, root_creation, terminal_quarantine, thread_creation,
    TaskRetireReason, MIN_LOAD_WEIGHT, SYSTEM_CLASS_WEIGHT_FLAG,
};
use super::*;
use crate::user::abi::UserAbi;
const ID: u64 = 85900;
fn boot() -> super::super::UserTaskBootstrap {
    let mut boot = root_creation::tests::bootstrap(UserAbi::Linux);
    boot.entry = x86_64::VirtAddr::new(crate::memory::paging::USER_SPACE_BASE + 0x400000);
    boot.stack_pointer = x86_64::VirtAddr::new(crate::memory::paging::USER_SPACE_BASE + 0x700000);
    boot
}
fn root(current: bool, f: impl FnOnce(usize, super::super::process_table::ProcessHandle)) {
    root_creation::tests::exec_root(
        ID,
        current,
        boot(),
        f,
    );
}
fn scope() -> DispatchPreemption<'static> {
    let mut scope = DispatchPreemption::enter_model(0);
    scope.mask_irqs();
    scope
}
#[test]
fn actual_current_self_demotion_caps_base_weight_without_revoking_original_reply_class() {
    root(true, |slot, handle| {
        let donor = thread_creation::tests::active_sibling(
            slot,
            ID + 1,
            boot(),
        );
        let scope = scope();
        let refs = identity::execution::count(slot);
        runqueue::weight::replace(slot, SYSTEM_CLASS_WEIGHT_FLAG | 4 * NICE_0_LOAD);
        assert!(demote_with(&scope));
        assert_eq!(runqueue::weight::value(slot), NICE_0_LOAD);
        assert!(!donation_publication::system_class_with(&scope, ID));
        assert_eq!(identity::execution::count(slot), refs);
        runqueue::weight::replace(donor, SYSTEM_CLASS_WEIGHT_FLAG | NICE_0_LOAD);
        assert!(donation_publication::bind_with(
            &scope,
            591,
            ID + 1,
            ID,
            true
        ));
        assert!(donation_publication::system_class_with(&scope, ID));
        runqueue::weight::replace(slot, SYSTEM_CLASS_WEIGHT_FLAG | MIN_LOAD_WEIGHT);
        assert!(demote_with(&scope));
        assert_eq!(runqueue::weight::value(slot), MIN_LOAD_WEIGHT);
        assert!(donation_publication::system_class_with(&scope, ID));
        assert!(donation_ledger::release_reply(
            591,
            super::super::ipc_donation::DonationNamespace::IpcReply
        ));
        assert!(!donation_publication::system_class_with(&scope, ID));
        drop(scope);
        thread_creation::tests::release_born(donor, ID + 1, handle);
    });
}
#[test]
fn actual_self_demotion_refuses_unmasked_foreign_cpu_and_sealed_or_missing_identity() {
    root(true, |slot, _| {
        runqueue::weight::replace(slot, SYSTEM_CLASS_WEIGHT_FLAG | 4 * NICE_0_LOAD);
        let weight = runqueue::weight::value(slot);
        let unmasked = DispatchPreemption::enter_model_with_enabled_irqs(0);
        assert!(!demote_with(&unmasked));
        assert_eq!(runqueue::weight::value(slot), weight);
        drop(unmasked);
        let foreign = DispatchPreemption::enter_model(1);
        assert!(!demote_with(&foreign));
        assert_eq!(runqueue::weight::value(slot), weight);
        drop(foreign);
        let scope = scope();
        let original = identity::read(slot).unwrap();
        identity::publish_metadata(slot, None);
        assert!(!demote_with(&scope));
        identity::publish_metadata(slot, Some(original));
        let stop =
            terminal_quarantine::begin(slot, Some(ID), TaskRetireReason::Exited, false).unwrap();
        assert!(!demote_with(&scope));
        assert_eq!(runqueue::weight::value(slot), weight);
        drop(stop);
    });
}
#[test]
fn actual_self_demotion_refuses_kernel_and_scalar_current_without_original_physical_bank() {
    root(false, |slot, _| {
        let scope = scope();
        assert!(!demote_with(&scope)); // actual physical kernel Current
        runqueue::weight::replace(slot, SYSTEM_CLASS_WEIGHT_FLAG | 4 * NICE_0_LOAD);
        let weight = runqueue::weight::value(slot);
        let modeled_scalar = cpu_local::install_test_current_owner(0, slot);
        assert!(!stack_resources::cpu_leases::owns_current(0, slot));
        assert!(!demote_with(&scope));
        assert_eq!(runqueue::weight::value(slot), weight);
        drop(modeled_scalar);
    });
}
