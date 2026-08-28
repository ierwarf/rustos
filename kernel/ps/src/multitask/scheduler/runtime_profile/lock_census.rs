//! Bounded lock-class and acquisition-site census rendering.
//!
//! - **Owner:** scheduler diagnostics own rendering; lockdep owns the atomic
//!   census and the selected class.
//! - **Boundary:** caller locations are compiler-provided static identities,
//!   never application-controlled pointers or strings.
//! - **State machine:** drain one class window, emit its ranked classes and
//!   selected-class sites, then select the next window's dominant class.
//! - **Invariants:** output is fixed-capacity, sorted, outside every tracked
//!   lock, and encodes file identity plus line without retaining references.
//! - **Concurrency:** destructive atomic drains transfer one complete window;
//!   the relaxed class selector is only a diagnostic hint.
//! - **Failure/recovery:** empty windows emit nothing, saturated counts remain
//!   bounded, and a missed window never affects scheduling authority.
//! - **Forbidden:** no allocation, debug output under a tracked lock, dynamic
//!   caller registry, or diagnostic value may influence scheduling policy.
//! - **Evidence:** `scheduler-dispatch`; focused threshold and hash witnesses
//!   are owned by `runtime_profile::tests`.

const LOCK_CLASS_NAMES: [&str; 6] = [
    "kernel-lock-class-0",
    "kernel-lock-class-1",
    "kernel-lock-class-2",
    "kernel-lock-class-3",
    "kernel-lock-class-4",
    "kernel-lock-class-5",
];

const LOCK_SITE_NAMES: [&str; 6] = [
    "kernel-lock-site-0",
    "kernel-lock-site-1",
    "kernel-lock-site-2",
    "kernel-lock-site-3",
    "kernel-lock-site-4",
    "kernel-lock-site-5",
];

/// Whether an acquisition site explains enough of the window to be worth a
/// debugcon record. The caller walks sites in descending count order.
pub(super) fn acquire_site_is_reportable(count: u64, acquisitions: u64) -> bool {
    count != 0 && count.saturating_mul(100) >= acquisitions
}

/// Stable 32-bit file identity for an acquisition site.
pub(super) fn fnv1a32(value: &str) -> u32 {
    let mut hash = 0x811c_9dc5_u32;
    for byte in value.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// Drains and renders the lock-class census outside scheduler custody.
pub(super) fn drain_class_and_site_census() {
    let lock_classes = nucleus_core::util::lockdep::work_budget::take_class_census();
    let mut ranked: [(usize, u64); nucleus_core::util::lockdep::MAX_LOCK_CLASSES] =
        [(0, 0); nucleus_core::util::lockdep::MAX_LOCK_CLASSES];
    for (index, count) in lock_classes.iter().copied().enumerate() {
        ranked[index] = (index, count);
    }
    ranked.sort_unstable_by(|left, right| right.1.cmp(&left.1));
    for (name, (class_index, count)) in LOCK_CLASS_NAMES.into_iter().zip(ranked) {
        if count == 0 {
            break;
        }
        crate::debug::record_milestone(
            crate::debug::LogCategory::Sched,
            name,
            count,
            class_index as u64,
        );
    }

    let censused_class = nucleus_core::util::lockdep::work_budget::site_census_class();
    let mut sites = nucleus_core::util::lockdep::work_budget::take_site_census();
    sites.sort_unstable_by(|left, right| right.2.cmp(&left.2));
    if censused_class != 0 && sites[0].2 != 0 {
        crate::debug::record_milestone(
            crate::debug::LogCategory::Sched,
            "kernel-lock-site-class",
            censused_class as u64,
            0,
        );
        for (name, (file, line, count)) in LOCK_SITE_NAMES.into_iter().zip(sites) {
            if count == 0 {
                break;
            }
            crate::debug::record_milestone(
                crate::debug::LogCategory::Sched,
                name,
                count,
                (u64::from(fnv1a32(file)) << 32) | u64::from(line),
            );
        }
    }

    let mut identity_sites =
        nucleus_core::util::lockdep::work_budget::take_identity_site_census();
    identity_sites.sort_unstable_by(|left, right| right.2.cmp(&left.2));
    const IDENTITY_SITE_NAMES: [&str; 6] = [
        "kernel-identity-site-0",
        "kernel-identity-site-1",
        "kernel-identity-site-2",
        "kernel-identity-site-3",
        "kernel-identity-site-4",
        "kernel-identity-site-5",
    ];
    for (name, (file, line, count)) in IDENTITY_SITE_NAMES.into_iter().zip(identity_sites) {
        if count == 0 {
            break;
        }
        crate::debug::record_milestone(
            crate::debug::LogCategory::Sched,
            name,
            count,
            (u64::from(fnv1a32(file)) << 32) | u64::from(line),
        );
    }

    // Rotate through the ranked classes rather than re-selecting the winner
    // every window. Pinning the top class meant five of the six ranked classes
    // never produced a single site, so the census could name the busiest class
    // but never say which lines made the others busy. One window each, in rank
    // order, covers all six without widening the fixed-capacity site table.
    let ranked_classes = ranked
        .iter()
        .take(LOCK_CLASS_NAMES.len())
        .filter(|(_, count)| *count != 0)
        .map(|(class_index, _)| *class_index);
    let next_class = ranked_classes
        .clone()
        .skip_while(|class_index| *class_index != censused_class)
        .nth(1)
        .or_else(|| ranked_classes.clone().next());
    if let Some(next_class) = next_class {
        nucleus_core::util::lockdep::work_budget::select_site_census_class(next_class);
    }
}
