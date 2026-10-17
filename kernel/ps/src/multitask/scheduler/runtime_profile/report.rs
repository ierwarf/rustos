//! Owner: consumed immutable counter window, never execution authority.
//! Boundary: projection/ranking and destructive diagnostic windows.
//! Lifecycle: one independent drain owns this window until the emitted report.
//! Concurrency: attribution is scalar telemetry, never a lifetime grant.
//! Failure: absent attribution uses zero IDs; no backing is reconstructed.
//! Forbidden: mutable Scheduler tables or frame/funding decisions.
//! Evidence: original profile regression witnesses and independent IRQ traces.
use super::*;

pub(super) fn project_window(
    window: owner::Counters, now: u64, period: u64,
    attribution: &dyn Fn(usize) -> (u64, u64), donations: u64, divergent: Option<usize>,
) -> SchedulerRuntimeProfile {
    let mut report = SchedulerRuntimeProfile::default();
    report.elapsed_ms = now.saturating_sub(window.runtime_profile_started_ticks)
        .saturating_mul(1000) / period.max(1);
    // A projection of consumed telemetry, never another mutable owner table.
    macro_rules! counters { ($($out:ident => $input:ident),* $(,)?) => {
        $(report.$out = window.$input;)*
    }; }
    counters!(
        entry_causes => runtime_profile_entry_counts,
        same_task_dispatches => runtime_profile_same_task_dispatches,
        task_switches => runtime_profile_task_switches,
        address_space_switches => runtime_profile_address_space_switches,
        cross_cpu_migrations => runtime_profile_cross_cpu_migrations,
        lock_acquisitions => runtime_profile_lock_acquisitions,
        lock_wait_ns => runtime_profile_lock_wait_ns,
        lock_hold_ns => runtime_profile_lock_hold_ns,
        lock_wait_max_ns => runtime_profile_lock_wait_max_ns,
        lock_hold_max_ns => runtime_profile_lock_hold_max_ns,
        lock_hold_max_caller => runtime_profile_lock_hold_max_caller,
        lock_hold_max_attributed_ns => runtime_profile_lock_hold_max_attributed_ns,
        deferred_wakes => runtime_profile_deferred_wakes,
        phase_ns => runtime_profile_phase_ns,
        runnable_samples => runtime_profile_runnable_samples,
    );
    (report.pick_scan_ns, report.pick_scan_calls) = super::super::locality::take_pick_scan_window();
    (report.handoff_scan_ns, report.handoff_scan_calls) = super::super::locality::take_handoff_scan_window();
    report.sync_handoff_hits = super::super::locality::take_sync_handoff_hit_window();
    report.sync_handoff_misses = super::super::locality::take_sync_handoff_miss_window();
    report.sync_handoff_stale = super::super::locality::take_sync_handoff_stale_window();
    report.sync_handoff_arms = super::super::locality::take_sync_handoff_arm_window();
    report.sync_handoff_reply_custody_fail = super::super::locality::take_sync_handoff_reply_custody_fail_window();
    report.sync_handoff_generation_fail_state = super::super::locality::take_sync_handoff_generation_fail_state_window();
    report.live_ipc_donations = donations;
    report.divergent_identity_slot = divergent.unwrap_or(usize::MAX);
    report.local_dispatch_shadow = local_shadow::take_observations();
    report.run_authority_divergence = super::super::super::run_authority::take_window().map(|(first, count)| {
        let slot = ((first >> 32) & 0xFFFF) as usize;
        (first, count, attribution(slot).0)
    });
    for slot in 0..MAX_TASK {
        let runtime_ns = window.runtime_profile_ns[slot];
        let dispatches = window.runtime_profile_dispatches[slot];
        report.total_runtime_ns = report.total_runtime_ns.saturating_add(runtime_ns);
        report.total_dispatches = report.total_dispatches.saturating_add(dispatches);
        if runtime_ns == 0 && dispatches == 0 { continue; }
        let (task_id, process_id) = attribution(slot);
        let row = SchedulerRuntimeProfileEntry { task_id, process_id, runtime_ns, dispatches };
        if let Some(index) = report.top.iter().position(|old|
            (row.runtime_ns, row.dispatches) > (old.runtime_ns, old.dispatches)) {
            report.top[index..].rotate_right(1);
            report.top[index] = row;
        }
    }
    report
}
