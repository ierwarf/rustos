//! Authoritative local-dispatch policy and catalog-fallback evidence.

#[cfg(rustos_scheduler_phase_profile)]
use super::{SchedulerRuntimeProfile, pack_u32_pair};

pub(super) fn take_observations() -> (u64, [u64; 7]) {
    #[cfg(rustos_scheduler_phase_profile)]
    return super::super::local_dispatch::drain_shadow_observations();
    #[cfg(not(rustos_scheduler_phase_profile))]
    (0, [0; 7])
}

#[cfg(rustos_scheduler_phase_profile)]
pub(super) fn emit_milestones(profile: &SchedulerRuntimeProfile) {
    let (qualified, reasons) = profile.local_dispatch_shadow;
    // arg0/arg1 keep the decoder's `qualified | matches` and
    // `mismatches | catalog_fallbacks` layout; the catalog comparison arm no
    // longer exists, so only `qualified` is observed.
    crate::debug::record_milestone(
        crate::debug::LogCategory::Sched,
        "kernel-scheduler-local-shadow",
        pack_u32_pair(qualified, 0),
        0,
    );
    if reasons.iter().any(|count| *count != 0) {
        crate::debug::record_milestone(
            crate::debug::LogCategory::Sched,
            "kernel-scheduler-local-shadow-fallback",
            pack_u32_pair(reasons[0], reasons[1]),
            pack_u32_pair(reasons[2], reasons[3]),
        );
        crate::debug::record_milestone(
            crate::debug::LogCategory::Sched,
            "kernel-scheduler-local-shadow-revalidate",
            pack_u32_pair(reasons[4], reasons[5]),
            reasons[6],
        );
    }
}
