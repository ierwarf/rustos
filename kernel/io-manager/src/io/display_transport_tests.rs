// SPDX-License-Identifier: MIT

use driver_domain_protocol::{DvmDisplayDamage, DvmGuiSurfacePoolHeader};

use super::{
    DvmPresentOutcome, SnapshotCopyPlan, contiguous_snapshot_copy_len, damage_bounds,
    header_fits_resource, snapshot_copy_plan, try_publish_full,
};

#[test]
fn pool_header_must_cover_all_three_slots() {
    let header = DvmGuiSurfacePoolHeader::new(32 * 1024 * 1024, 1600, 900);
    assert!(header_fits_resource(header, 32 * 1024 * 1024));
    assert!(!header_fits_resource(header, header.region_bytes - 1));
}

#[test]
fn damage_bounds_reject_overflow_and_accept_full_frame() {
    assert_eq!(
        damage_bounds(driver_domain_protocol::DvmDisplayDamage::full(), 1600, 900),
        Some((0, 0, 1600, 900))
    );
    assert_eq!(
        damage_bounds(
            driver_domain_protocol::DvmDisplayDamage::rect(1599, 899, 2, 1),
            1600,
            900
        ),
        None
    );
}

#[test]
fn exact_predecessor_snapshot_copies_only_declared_damage() {
    let damage = DvmDisplayDamage::rect(100, 200, 32, 48);
    assert_eq!(
        snapshot_copy_plan(damage, 1600, 900, 40, 40, true),
        Some(SnapshotCopyPlan {
            x: 100,
            y: 200,
            width: 32,
            height: 48,
            incremental: true,
        })
    );
}

#[test]
fn stale_or_replaced_snapshot_forces_a_complete_copy() {
    let damage = DvmDisplayDamage::rect(100, 200, 32, 48);
    let complete = Some(SnapshotCopyPlan {
        x: 0,
        y: 0,
        width: 1600,
        height: 900,
        incremental: false,
    });
    assert_eq!(
        snapshot_copy_plan(damage, 1600, 900, 38, 40, true),
        complete
    );
    assert_eq!(
        snapshot_copy_plan(damage, 1600, 900, 40, 40, false),
        complete
    );
    assert_eq!(
        snapshot_copy_plan(DvmDisplayDamage::full(), 1600, 900, 40, 40, true),
        complete
    );
}

#[test]
fn full_width_snapshot_uses_one_bounded_bulk_copy() {
    let full = SnapshotCopyPlan {
        x: 0,
        y: 0,
        width: 1600,
        height: 900,
        incremental: false,
    };
    assert_eq!(
        contiguous_snapshot_copy_len(full, 1600, 1600 * 4),
        Some(1600 * 900 * 4)
    );
    assert_eq!(
        contiguous_snapshot_copy_len(full, 1600, 1600 * 4 + 64),
        Some((1600 * 4 + 64) * 900)
    );

    let partial = SnapshotCopyPlan {
        x: 10,
        y: 20,
        width: 30,
        height: 40,
        incremental: true,
    };
    assert_eq!(contiguous_snapshot_copy_len(partial, 1600, 1600 * 4), None);
}

#[test]
fn missing_gui_dvm_is_unavailable_not_a_fallback_provider() {
    assert_eq!(
        try_publish_full(core::ptr::null(), 1, 1, 4),
        DvmPresentOutcome::Unavailable
    );
}
