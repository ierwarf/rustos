//! Unit witnesses for lock-class tracking, dense CPU identity, and the raw
//! guard's preemption accounting.
//!
//! Split out of `lockdep.rs` when that file crossed its line budget. The module
//! path is unchanged, so the `util::lockdep::tests::*` witnesses in
//! `formal/run-source-conformance.sh` and `formal/system-flows.tsv` still name
//! these tests.

use super::cpu_identity::{decode_cpu_token, select_cpu_index};
use super::{
    graph_reaches, guard_release_is_admissible, preemption_release_is_admissible,
    preemption_units_match,
};

#[test]
fn occupancy_bits_are_visited_in_ascending_slot_order() {
    let mut bits = (1_u64 << 0) | (1 << 3) | (1 << 63);
    let mut seen = alloc::vec::Vec::new();
    while let Some(bit) = super::next_occupied_bit(&mut bits) {
        seen.push(bit);
    }
    assert_eq!(seen, [0, 3, 63]);
    assert_eq!(bits, 0);
}

#[test]
fn an_empty_occupancy_word_visits_nothing() {
    // This is the common case: no task holds a sleepable class, so a
    // tracked spin acquisition must not walk the stack table at all.
    let mut bits = 0_u64;
    assert_eq!(super::next_occupied_bit(&mut bits), None);
}

#[test]
fn dependency_walk_detects_transitive_cycle_edge() {
    let mut rows = [0_u64; 64];
    rows[1] = 1 << 2;
    rows[2] = 1 << 3;
    assert!(graph_reaches(1, 3, |node| rows[node]));
    assert!(!graph_reaches(3, 1, |node| rows[node]));
    rows[3] = 1 << 1;
    assert!(graph_reaches(3, 2, |node| rows[node]));
}

#[test]
fn dense_apic_identity_map_does_not_index_by_raw_apic_id() {
    let identities = [1_u64, u64::from(0x1234_u32) + 1, 8];
    assert_eq!(select_cpu_index(identities, 0), Some(0));
    assert_eq!(select_cpu_index(identities, 0x1234), Some(1));
    assert_eq!(select_cpu_index(identities, 7), Some(2));
    assert_eq!(select_cpu_index(identities, 2), None);
    assert_eq!(decode_cpu_token(0, 3), None);
    assert_eq!(decode_cpu_token(1, 3), Some(0));
    assert_eq!(decode_cpu_token(3, 3), Some(2));
    assert_eq!(decode_cpu_token(4, 3), None);
}

#[test]
fn tracked_guard_release_requires_same_cpu_apic_and_positive_depth() {
    assert!(guard_release_is_admissible(1, 0x1234, 1, 0x1234, 1));
    assert!(guard_release_is_admissible(1, 0x1234, 1, 0x1234, 3));
    assert!(!guard_release_is_admissible(1, 0x1234, 0, 0, 1));
    assert!(!guard_release_is_admissible(1, 0x1234, 1, 0x4321, 1));
    assert!(!guard_release_is_admissible(1, 0x1234, 1, 0x1234, 0));
}

#[test]
fn pending_acquire_units_cannot_consume_a_held_guard_pin() {
    assert!(preemption_units_match(1, 1, 0));
    assert!(preemption_units_match(1, 0, 1));
    assert!(preemption_units_match(2, 1, 1));
    assert!(!preemption_units_match(0, 1, 0));
    assert!(preemption_release_is_admissible(2, 1, 0));
    assert!(preemption_release_is_admissible(2, 0, 1));
    assert!(!preemption_release_is_admissible(1, 1, 0));
}

/// The raw-spin acquire path is handed its logical CPU index and must not
/// ask the hardware for it again.
///
/// This is a source witness rather than a declared ceiling because the path
/// runs with interrupts enabled: a dynamic count over it also collects the
/// derivations of anything that interrupts it, including the context-switch
/// commit the timer stub runs after the IRQ guard has already been dropped.
/// The property being pinned is static anyway -- whether these functions
/// call `current_cpu_index()` or take a `cpu` argument -- so counting was
/// never the right instrument for it.
///
/// `record_irq_usage` is the one that regressed: it re-derived the index
/// from inside `before_acquire_with_irq_tracking`, which had the answer in
/// a register, on every tracked lock acquisition in the kernel.
#[test]
fn the_raw_acquire_path_never_rederives_the_cpu_index() {
    let source = include_str!("../lockdep.rs");
    let start = source
        .find("fn before_acquire_with_irq_tracking(")
        .expect("the raw acquire path must still be in this file");
    let end = source[start..]
        .find("\n}\n")
        .map(|offset| start + offset)
        .expect("the raw acquire path must be a complete function");
    let body = &source[start..end];
    assert!(
        !body.contains("current_cpu_index()"),
        "the raw acquire path derived its own index again:\n{body}"
    );
    assert!(
        body.contains("record_irq_usage(cpu, class_index, acquire_site)"),
        "the IRQ-usage record must be handed the index, not derive it"
    );

    let graph = include_str!("dependency_graph.rs");
    assert!(
        !graph.contains("irq_context_depth()"),
        "the dependency graph must take a derived index, not derive one"
    );
}
