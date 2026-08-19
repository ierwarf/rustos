use std::path::Path;

use super::{BTreeMap, BTreeSet, FlowGraph, cyclic_scc_count, is_unregistered_high_risk_source};

#[test]
fn strongly_connected_cycle_with_terminal_exit_is_counted_once() {
    let nodes = BTreeSet::from(["START", "a", "b", "done"]);
    let outgoing = BTreeMap::from([
        ("START", BTreeSet::from(["a"])),
        ("a", BTreeSet::from(["b"])),
        ("b", BTreeSet::from(["a", "done"])),
    ]);
    let incoming = BTreeMap::from([
        ("a", BTreeSet::from(["START", "b"])),
        ("b", BTreeSet::from(["a"])),
        ("done", BTreeSet::from(["b"])),
    ]);
    let graph = FlowGraph {
        transitions: Vec::new(),
        nodes,
        outgoing,
        incoming,
    };
    assert_eq!(cyclic_scc_count(&graph), 1);
}

#[test]
fn high_risk_fallback_targets_stateful_boundaries_not_every_rust_file() {
    assert!(is_unregistered_high_risk_source(Path::new(
        "kernel/ps/src/multitask/scheduler.rs"
    )));
    assert!(is_unregistered_high_risk_source(Path::new(
        "services/newpolicyd/src/main.rs"
    )));
    assert!(is_unregistered_high_risk_source(Path::new(
        "kernel/compat/src/user/syscall/linux/new_broker_ops.rs"
    )));
    assert!(!is_unregistered_high_risk_source(Path::new(
        "services/uiserver/src/color.rs"
    )));
    assert!(!is_unregistered_high_risk_source(Path::new(
        "kernel/ps/src/debug_format.rs"
    )));
}
