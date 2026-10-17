//! Decode local policy observations separately from dispatch authority.

use super::{hex_field, milestone_name};

/// Counts include diagnostic-only observations of catalog-owned arms.
#[derive(Default)]
pub(super) struct LocalShadowTotal {
    pub(super) qualified: u128,
    pub(super) matches: u128,
    pub(super) mismatches: u128,
    pub(super) catalog_fallbacks: u128,
    pub(super) fallbacks: [u128; 7],
    pub(super) last_mismatch: Option<(u8, u8, u8, u8, u8, u8)>,
    pub(super) last_mismatch_vruntime: Option<(u64, u64)>,
    pub(super) catalog_recheck: Option<u8>,
    pub(super) catalog_admission: Option<u8>,
}

fn packed_high(value: u64) -> u64 {
    value >> 32
}

fn packed_low(value: u64) -> u64 {
    value & u64::from(u32::MAX)
}

/// Accumulates the three scheduler shadow milestones. Returns `true` only for
/// a recognized, complete record; unrelated debug milestones stay ignored.
pub(super) fn parse_milestone(line: &str, total: &mut LocalShadowTotal) -> bool {
    let Some(name) = milestone_name(line) else {
        return false;
    };
    let Some(arg0) = hex_field(line, "arg0=") else {
        return false;
    };
    let Some(arg1) = hex_field(line, "arg1=") else {
        return false;
    };
    match name {
        "kernel-scheduler-local-shadow" => {
            total.qualified += u128::from(packed_high(arg0));
            total.matches += u128::from(packed_low(arg0));
            total.mismatches += u128::from(packed_high(arg1));
            total.catalog_fallbacks += u128::from(packed_low(arg1));
        }
        "kernel-scheduler-local-shadow-fallback" => {
            total.fallbacks[0] += u128::from(packed_high(arg0));
            total.fallbacks[1] += u128::from(packed_low(arg0));
            total.fallbacks[2] += u128::from(packed_high(arg1));
            total.fallbacks[3] += u128::from(packed_low(arg1));
        }
        "kernel-scheduler-local-shadow-revalidate" => {
            total.fallbacks[4] += u128::from(packed_high(arg0));
            total.fallbacks[5] += u128::from(packed_low(arg0));
            total.fallbacks[6] += u128::from(arg1);
        }
        "kernel-scheduler-local-shadow-mismatch" => {
            let extended = (arg0 >> 56) & 1 == 1;
            total.catalog_recheck = extended.then_some((arg0 >> 48) as u8);
            total.catalog_admission = extended.then_some((arg0 >> 57) as u8);
            total.last_mismatch = Some((
                (arg0 >> 40) as u8,
                (arg0 >> 32) as u8,
                (arg0 >> 24) as u8,
                (arg0 >> 16) as u8,
                (arg0 >> 8) as u8,
                arg0 as u8,
            ));
        }
        "kernel-scheduler-local-shadow-mismatch-vruntime" => {
            total.last_mismatch_vruntime = Some((arg0, arg1));
        }
        _ => return false,
    }
    true
}

/// Renders counts separately from timing phases: these are correctness
/// evidence and must never be presented as latency attribution.
pub(super) fn render(total: &LocalShadowTotal) -> String {
    let observed = total.qualified
        + total.matches
        + total.mismatches
        + total.catalog_fallbacks
        + total.fallbacks.iter().sum::<u128>();
    if observed == 0 {
        return String::new();
    }
    const FALLBACK_NAMES: [&str; 7] = [
        "atomic-activation",
        "synchronous-handoff",
        "no-local-candidate",
        "publication-unavailable",
        "lifecycle-ineligible",
        "custody-unavailable",
        "frame-unavailable",
    ];
    let mut out = format!(
        "\nlocal-dispatch observations (system-wide): qualified={} catalog_policy_matches={} policy_differences={} catalog_fallbacks={}\n",
        total.qualified, total.matches, total.mismatches, total.catalog_fallbacks
    );
    for (name, count) in FALLBACK_NAMES.iter().zip(total.fallbacks) {
        if count != 0 {
            out.push_str(&format!("  fallback.{name}={count}\n"));
        }
    }
    if let Some((flags, cpu, current, catalog, shadow, selected)) = total.last_mismatch {
        out.push_str(&format!(
            "  last-policy-difference.cpu={cpu} current={current} catalog={catalog} local={shadow} selected={selected} flags=0x{flags:02x}\n"
        ));
    }
    if let Some(slot) = total.catalog_recheck {
        out.push_str(&format!(
            "  last-policy-difference.catalog-recheck={slot}\n"
        ));
    }
    if let Some(reason) = total.catalog_admission {
        out.push_str(&format!(
            "  last-policy-difference.catalog-yield-admission={reason}\n"
        ));
    }
    if let Some((local, catalog)) = total.last_mismatch_vruntime {
        out.push_str(&format!(
            "  last-policy-difference.vruntime.local={local} catalog={catalog} locality_lag={}\n",
            local.saturating_sub(catalog)
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_choice_is_not_reported_as_the_dispatched_slot() {
        let mut total = LocalShadowTotal::default();
        assert!(parse_milestone(
            "name=kernel-scheduler-local-shadow-mismatch arg0=0x011dc10001021d02 arg1=0x0",
            &mut total,
        ));
        assert_eq!(total.catalog_recheck, Some(29));
        assert_eq!(total.catalog_admission, Some(0));
        let text = render(&total);
        // render requires counters as well as a witness.
        assert!(text.is_empty());
        total.mismatches = 1;
        let text = render(&total);
        assert!(text.contains("catalog=2 local=29 selected=2"), "{text}");
        assert!(text.contains("catalog-recheck=29"), "{text}");
        assert!(parse_milestone(
            "name=kernel-scheduler-local-shadow-mismatch arg0=0xc10001021d02 arg1=0x0",
            &mut total,
        ));
        assert_eq!(total.catalog_recheck, None);
        assert_eq!(total.catalog_admission, None);
    }

    #[test]
    fn decodes_all_shadow_count_records() {
        let mut total = LocalShadowTotal::default();
        assert!(parse_milestone(
            "name=kernel-scheduler-local-shadow arg0=0x0000000200000003 arg1=0x0000000400000005",
            &mut total,
        ));
        assert!(parse_milestone(
            "name=kernel-scheduler-local-shadow-fallback arg0=0x0000000600000007 arg1=0x0000000800000009",
            &mut total,
        ));
        assert!(parse_milestone(
            "name=kernel-scheduler-local-shadow-revalidate arg0=0x0000000a0000000b arg1=0xc",
            &mut total,
        ));
        assert_eq!(total.qualified, 2);
        assert_eq!(total.matches, 3);
        assert_eq!(total.mismatches, 4);
        assert_eq!(total.catalog_fallbacks, 5);
        assert!(parse_milestone(
            "name=kernel-scheduler-local-shadow-mismatch arg0=0x0000120d0e0f1011 arg1=0x0",
            &mut total,
        ));
        assert!(parse_milestone(
            "name=kernel-scheduler-local-shadow-mismatch-vruntime arg0=0x32 arg1=0x20",
            &mut total,
        ));
        assert_eq!(total.fallbacks, [6, 7, 8, 9, 10, 11, 12]);
        assert_eq!(total.last_mismatch, Some((18, 13, 14, 15, 16, 17)));
        assert_eq!(total.last_mismatch_vruntime, Some((50, 32)));
        let rendered = render(&total);
        assert!(rendered.contains("policy_differences=4"), "{rendered}");
        assert!(
            rendered.contains("fallback.frame-unavailable=12"),
            "{rendered}"
        );
        assert!(
            rendered.contains(
                "last-policy-difference.cpu=13 current=14 catalog=15 local=16 selected=17 flags=0x12"
            ),
            "{rendered}"
        );
        assert!(rendered.contains("vruntime.local=50 catalog=32 locality_lag=18"));
    }
}
