//! Identifier-registry checks for the driver-domain wire protocols.
//!
//! Each protocol owns one operation/kind number space and one version
//! constant. These tests only enforce what is already published: no two
//! operations of one protocol share a number, every protocol exposes its
//! version, and a newly declared kind constant cannot be added without
//! registering it here (which is where the collision would be noticed).
extern crate std;

use super::*;
use std::collections::BTreeSet;
use std::string::String;
use std::vec::Vec;

fn assert_unique(protocol: &str, ops: &[(&str, u64)]) {
    let mut seen = BTreeSet::new();
    for (name, number) in ops {
        assert!(
            seen.insert(*number),
            "{protocol}: operation {name} reuses number {number}"
        );
    }
}

/// `(protocol, [(name, number)])` for every constant-declared number space.
fn constant_spaces() -> Vec<(&'static str, &'static str, Vec<(&'static str, u64)>)> {
    std::vec![
        (
            "gui-surface",
            "DVM_GUI_SURFACE_KIND_",
            std::vec![
                ("PRESENT", DVM_GUI_SURFACE_KIND_PRESENT as u64),
                ("RELEASE", DVM_GUI_SURFACE_KIND_RELEASE as u64),
                ("FOCUS", DVM_GUI_SURFACE_KIND_FOCUS as u64),
            ]
        ),
        (
            "gpu-command",
            "DVM_GPU_COMMAND_KIND_",
            std::vec![
                ("CLEAR", DVM_GPU_COMMAND_KIND_CLEAR as u64),
                ("SOLID_QUAD", DVM_GPU_COMMAND_KIND_SOLID_QUAD as u64),
                ("TEXTURED_QUAD", DVM_GPU_COMMAND_KIND_TEXTURED_QUAD as u64),
            ]
        ),
        (
            "input-frame",
            "RUSTOS_INPUT_KIND_",
            std::vec![
                ("SESSION_START", RUSTOS_INPUT_KIND_SESSION_START as u64),
                ("KEY", RUSTOS_INPUT_KIND_KEY as u64),
                ("POINTER", RUSTOS_INPUT_KIND_POINTER as u64),
                ("SESSION_END", RUSTOS_INPUT_KIND_SESSION_END as u64),
                ("POINTER_POSITION", RUSTOS_INPUT_KIND_POINTER_POSITION as u64),
            ]
        ),
    ]
}

#[test]
fn operation_numbers_are_unique_within_each_protocol() {
    for (protocol, _, ops) in constant_spaces() {
        assert_unique(protocol, &ops);
    }
}

#[test]
fn every_declared_kind_constant_is_registered() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for (protocol, prefix, ops) in constant_spaces() {
        let mut declared = 0;
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            declared += text
                .lines()
                .filter(|line| {
                    line.strip_prefix("pub const ")
                        .is_some_and(|rest| rest.starts_with(prefix) && rest.contains(": u"))
                })
                .count();
        }
        assert_eq!(
            declared,
            ops.len(),
            "{protocol}: a {prefix}* constant is declared but not registered in abi_registry_tests.rs"
        );
    }
}

#[test]
fn block_operation_numbers_are_unique_and_closed() {
    let all = [
        DvmBlockOperation::Read,
        DvmBlockOperation::Write,
        DvmBlockOperation::Flush,
        DvmBlockOperation::Discard,
        DvmBlockOperation::WriteZeroes,
    ];
    let ops: Vec<(String, u64)> = all
        .iter()
        .map(|op| (std::format!("{op:?}"), op.wire() as u64))
        .collect();
    let borrowed: Vec<(&str, u64)> = ops.iter().map(|(n, v)| (n.as_str(), *v)).collect();
    assert_unique("block", &borrowed);
    for op in all {
        assert_eq!(DvmBlockOperation::decode(op.wire()), Some(op));
    }
    // A variant added without updating `all` above decodes here and fails.
    let decodable = (0..256u32)
        .filter(|value| DvmBlockOperation::decode(*value).is_some())
        .count();
    assert_eq!(decodable, all.len(), "DvmBlockOperation gained a variant; list it above");
}

/// Every protocol exposes a version constant; this is the registry of them.
/// A bump is a semantic break and must be paired with the contract change.
#[test]
fn every_protocol_exposes_its_version_constant() {
    let versions: &[(&str, u64)] = &[
        ("app-buffer", DVM_APP_BUFFER_VERSION as u64),
        ("display", DVM_DISPLAY_VERSION as u64),
        ("block", DVM_BLOCK_VERSION as u64),
        ("gui-surface", DVM_GUI_SURFACE_VERSION as u64),
        ("gui-surface-pool", DVM_GUI_SURFACE_POOL_VERSION as u64),
        ("net", DVM_NET_VERSION as u64),
        ("input-ring", DVM_INPUT_RING_VERSION as u64),
        ("input-frame", RUSTOS_INPUT_VERSION as u64),
        ("gpu-render", DVM_GPU_RENDER_VERSION as u64),
        ("gpu-prime-completion", DVM_GPU_PRIME_COMPLETION_VERSION as u64),
        ("gpu-atlas-transport", DVM_GPU_ATLAS_TRANSPORT_VERSION as u64),
    ];
    for (protocol, version) in versions {
        assert!(*version >= 1, "{protocol}: version 0 is reserved");
    }
    // Each protocol module must declare at least one *_VERSION constant, so a
    // new protocol file cannot ship without joining the table above.
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut declared = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let text = std::fs::read_to_string(entry.unwrap().path()).unwrap();
        declared += text
            .lines()
            .filter(|line| {
                line.strip_prefix("pub const ").is_some_and(|rest| {
                    let name = rest.split(':').next().unwrap_or("");
                    name.ends_with("_VERSION") && name.starts_with(|c: char| c.is_ascii_uppercase())
                })
            })
            .count();
    }
    assert_eq!(declared, versions.len(), "a *_VERSION constant is not in the registry above");
}
