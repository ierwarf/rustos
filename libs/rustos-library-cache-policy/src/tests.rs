use super::*;
use std::{format, string::ToString};

fn header(policy: Policy) -> std::string::String {
    format!(
        "{MANIFEST_VERSION}\t{}\t{}\t{}\t{}\t{}\n",
        policy.budget_bytes,
        policy.max_file_bytes,
        policy.max_entries,
        policy.min_applications,
        policy.read_ahead_pages
    )
}
fn row(path: &str, bytes: u64) -> std::string::String {
    format!("{bytes}\t{}\t{path}\n", "ab".repeat(32))
}

#[test]
fn common_small_is_warm_rare_and_large_stay_lazy() {
    let mut select = Selection::new(Policy::DEFAULT).unwrap();
    assert!(matches!(
        select.consider(2_000_001, 1, true),
        Admission::Warm { .. }
    ));
    assert_eq!(
        select.consider(4096, 1, false),
        Admission::Lazy(LazyReason::Rare)
    );
    assert_eq!(
        select.consider(5 * 1024 * 1024, 100, true),
        Admission::Lazy(LazyReason::Large)
    );
    assert!(matches!(
        select.consider(4096, 2, false),
        Admission::Warm { .. }
    ));
    assert_eq!(select.selected_entries(), 2);
    assert!(is_common_runtime("libc.so.6"));
    assert!(!is_common_runtime("libQt6Core.so.6"));
    assert!(!is_common_runtime("libc.so.6.extra"));
}

#[test]
fn budget_counts_pages_not_file_lengths_and_rejection_does_not_spend() {
    let policy = Policy {
        budget_bytes: 8192,
        max_file_bytes: 8192,
        ..Policy::DEFAULT
    };
    let mut select = Selection::new(policy).unwrap();
    assert_eq!(
        select.consider(4097, 2, false),
        Admission::Warm {
            resident_bytes: 8192
        }
    );
    assert_eq!(
        select.consider(1, 2, false),
        Admission::Lazy(LazyReason::Budget)
    );
    assert_eq!(select.selected_bytes(), 8192);
    assert_eq!(select.selected_entries(), 1);
    assert_eq!(resident_bytes(u64::MAX), None);
}

#[test]
fn rejects_big_candidate_without_blocking_later_small_one() {
    let policy = Policy {
        budget_bytes: 8192,
        max_file_bytes: 8192,
        ..Policy::DEFAULT
    };
    let mut select = Selection::new(policy).unwrap();
    select.consider(4096, 2, false);
    assert_eq!(
        select.consider(8192, 2, false),
        Admission::Lazy(LazyReason::Budget)
    );
    assert_eq!(
        select.consider(4096, 2, false),
        Admission::Warm {
            resident_bytes: 4096
        }
    );
}

#[test]
fn entry_limit_is_an_explicit_lazy_reason() {
    let mut select = Selection::new(Policy {
        max_entries: 1,
        ..Policy::DEFAULT
    })
    .unwrap();
    select.consider(1, 2, false);
    assert_eq!(
        select.consider(1, 2, false),
        Admission::Lazy(LazyReason::EntryLimit)
    );
    assert_eq!(
        select.consider(0, 2, false),
        Admission::Lazy(LazyReason::Empty)
    );
}

#[test]
fn parses_a_borrowed_bounded_plan() {
    let text = header(Policy::DEFAULT) + &row("/usr/lib/libc.so.6", 4097);
    let plan = Plan::parse(&text).unwrap();
    assert_eq!(plan.policy, Policy::DEFAULT);
    assert_eq!(plan.entry_count(), 1);
    assert_eq!(plan.resident_bytes(), 8192);
    let entry = plan.entries().next().unwrap().unwrap();
    assert_eq!(entry.path, "/usr/lib/libc.so.6");
    assert_eq!(entry.bytes, 4097);
    assert_eq!(entry.sha256, [0xab; 32]);
    assert_eq!(
        Plan::parse(&header(Policy::DEFAULT)).unwrap().entry_count(),
        0
    );
}

#[test]
fn plan_rejects_duplicate_paths_and_overbudget_after_rounding() {
    let text = header(Policy::DEFAULT) + &row("/lib/a.so", 1) + &row("/lib/a.so", 2);
    assert_eq!(Plan::parse(&text).unwrap_err(), Error::Duplicate);
    let small = Policy {
        budget_bytes: 4096,
        max_file_bytes: 4096,
        ..Policy::DEFAULT
    };
    let text = header(small) + &row("/lib/a.so", 1) + &row("/lib/b.so", 1);
    assert_eq!(Plan::parse(&text).unwrap_err(), Error::Budget);
}

#[test]
fn paths_cannot_escape_or_smuggle_records() {
    for path in [
        "lib/x.so",
        "/",
        "/lib/../x.so",
        "/./x.so",
        "/lib//x.so",
        "/lib/x.so/",
        "/lib/x\t.so",
        "/lib/x\n.so",
        "/lib/x\0.so",
        "/lib/x\r.so",
    ] {
        assert!(!valid_path(path), "accepted {path:?}");
        assert!(Plan::parse(&(header(Policy::DEFAULT) + &row(path, 1))).is_err());
    }
    assert!(valid_path("/usr/lib/a space.so"));
    assert!(valid_path("/usr/lib/문자.so"));
    assert!(!valid_path(&format!("/{}", "a".repeat(MAX_PATH_BYTES))));
}

#[test]
fn malformed_digest_and_numeric_overflow_never_truncate() {
    let good = row("/lib/a.so", 1);
    for broken in [
        good.replacen("ab", "AB", 1),
        good.replacen("ab", "g0", 1),
        good.replacen("1\t", "18446744073709551616\t", 1),
        good.replacen("1\t", "+1\t", 1),
        good.replacen("1\t", "01\t", 1),
    ] {
        assert!(Plan::parse(&(header(Policy::DEFAULT) + &broken)).is_err());
    }
    assert!(Plan::parse(&(header(Policy::DEFAULT) + &good + "\n")).is_err());
    assert!(Plan::parse(&(header(Policy::DEFAULT) + &good + "extra")).is_err());
    assert!(Plan::parse(&(header(Policy::DEFAULT) + &good.replace('\n', "\r\n"))).is_err());
    assert!(Plan::parse(&(header(Policy::DEFAULT) + good.trim_end())).is_err());
}

#[test]
fn invalid_configuration_is_not_silently_clamped() {
    assert_eq!(
        MAX_BUDGET_BYTES / PAGE_BYTES,
        rustos_user_abi::syscall::FRAME_SOURCE_MAX_LIVE as u64
    );
    assert!(Policy::DEFAULT.budget_bytes <= MAX_BUDGET_BYTES / 2);
    for policy in [
        Policy {
            budget_bytes: 0,
            ..Policy::DEFAULT
        },
        Policy {
            max_file_bytes: MAX_BUDGET_BYTES + 1,
            ..Policy::DEFAULT
        },
        Policy {
            max_entries: MAX_WARM_ENTRIES + 1,
            ..Policy::DEFAULT
        },
        Policy {
            min_applications: 0,
            ..Policy::DEFAULT
        },
        Policy {
            read_ahead_pages: 17,
            ..Policy::DEFAULT
        },
    ] {
        assert!(Selection::new(policy).is_err());
        assert!(Plan::parse(&header(policy)).is_err());
    }
    assert!(Plan::parse(&"a".repeat(MAX_MANIFEST_BYTES + 1)).is_err());
    assert!(Plan::parse("wrong-version\n").is_err());
    assert!(Plan::parse(&header(Policy::DEFAULT).trim_end().to_string()).is_err());
}
