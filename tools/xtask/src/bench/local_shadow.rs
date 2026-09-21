//! Decode and render authoritative local-dispatch policy evidence.

use super::{hex_field, milestone_name};

/// Counts emitted by the authoritative local scheduler picker.
#[derive(Default)]
pub(super) struct LocalShadowTotal {
    pub(super) qualified: u128,
    pub(super) matches: u128,
    pub(super) mismatches: u128,
    pub(super) catalog_fallbacks: u128,
    pub(super) fallbacks: [u128; 7],
    pub(super) last_mismatch: Option<(u8, u8, u8, u8, u8, u8)>,
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
            total.last_mismatch = Some((
                (arg0 >> 40) as u8,
                (arg0 >> 32) as u8,
                (arg0 >> 24) as u8,
                (arg0 >> 16) as u8,
                (arg0 >> 8) as u8,
                arg0 as u8,
            ));
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
        "\nlocal-dispatch authority (system-wide): qualified={} catalog_policy_matches={} policy_differences={} catalog_fallbacks={}\n",
        total.qualified, total.matches, total.mismatches, total.catalog_fallbacks
    );
    for (name, count) in FALLBACK_NAMES.iter().zip(total.fallbacks) {
        if count != 0 {
            out.push_str(&format!("  fallback.{name}={count}\n"));
        }
    }
    if let Some((flags, cpu, current, catalog, shadow, _catalog_repeat)) = total.last_mismatch {
        out.push_str(&format!(
            "  last-policy-difference.cpu={cpu} current={current} catalog={catalog} local={shadow} selected={shadow} flags=0x{flags:02x}\n"
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(total.fallbacks, [6, 7, 8, 9, 10, 11, 12]);
        assert_eq!(total.last_mismatch, Some((18, 13, 14, 15, 16, 17)));
        let rendered = render(&total);
        assert!(rendered.contains("policy_differences=4"), "{rendered}");
        assert!(
            rendered.contains("fallback.frame-unavailable=12"),
            "{rendered}"
        );
        assert!(
            rendered.contains(
                "last-policy-difference.cpu=13 current=14 catalog=15 local=16 selected=16 flags=0x12"
            ),
            "{rendered}"
        );
    }
}
