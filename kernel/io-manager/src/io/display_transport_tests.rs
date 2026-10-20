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

static TRANSPORT_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn installation_invites_the_gpu_peer_before_publication_without_a_cpu_frame() {
    use super::*;

    let _serial = TRANSPORT_TEST_LOCK.lock().unwrap();
    let mut header = alloc::vec![0_u8; 4096];
    let mut doorbell = alloc::vec![0_u32; 4];
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            INSTALLED.store(false, Ordering::Release);
            TRANSPORT_REVOKED.store(false, Ordering::Release);
            SHARED_HEADER_ADDR.store(0, Ordering::Release);
            SHARED_DOORBELL_ADDR.store(0, Ordering::Release);
            GUI_DVM_EXPECTED_INVITATION.store(0, Ordering::Release);
        }
    }
    let _restore = Restore;
    SHARED_HEADER_ADDR.store(header.as_mut_ptr() as usize, Ordering::Release);
    SHARED_DOORBELL_ADDR.store(doorbell.as_mut_ptr() as usize, Ordering::Release);
    INSTALLED.store(true, Ordering::Release);

    // The exclusive install transaction must advertise before consumers can
    // publish a competing invitation. Exercise its actual MMIO helper below.
    let installation = include_str!("dvm_display/installation.rs");
    let invite = installation.find("    signal_gui_dvm(2);").expect("initial prime invitation");
    let publish = installation.find("    INSTALLED.store(true,").expect("transport publication");
    assert!(invite < publish, "initial invitation must precede consumer admission");
    signal_gui_dvm(2);
    assert!(gpu_atlas_info().is_none(), "invitation alone is not readiness");
    assert_eq!(read_u64(DVM_GUI_SURFACE_POOL_INVITATION_OFFSET), 2);
    assert_eq!(GUI_DVM_EXPECTED_INVITATION.load(Ordering::Acquire), 2);
    assert_eq!(doorbell[IVSHMEM_DOORBELL_OFFSET / 4], (GUI_DVM_PEER_ID << 16) as u32);
    assert_eq!(PUBLISHED_GENERATION.load(Ordering::Acquire), 0);
    assert!(SLOT_STATE.iter().all(|state| state.load(Ordering::Acquire) == GUI_SLOT_FREE));
    assert_eq!(read_u64(DVM_GUI_SURFACE_POOL_READY_CONFIRMATION_OFFSET), 0);

    // A later exact invitation is not rewritten by repeated discovery.
    write_u64(DVM_GUI_SURFACE_POOL_INVITATION_OFFSET, 6);
    GUI_DVM_EXPECTED_INVITATION.store(6, Ordering::Release);
    assert!(gpu_atlas_info().is_none());
    assert_eq!(read_u64(DVM_GUI_SURFACE_POOL_INVITATION_OFFSET), 6);

    GUI_DVM_EXPECTED_INVITATION.store(0, Ordering::Release);
    write_u64(DVM_GUI_SURFACE_POOL_INVITATION_OFFSET, 0);
    TRANSPORT_REVOKED.store(true, Ordering::Release);
    assert!(gpu_atlas_info().is_none());
    assert_eq!(read_u64(DVM_GUI_SURFACE_POOL_INVITATION_OFFSET), 0);
    TRANSPORT_REVOKED.store(false, Ordering::Release);
    INSTALLED.store(false, Ordering::Release);
    assert!(gpu_atlas_info().is_none());
    assert_eq!(read_u64(DVM_GUI_SURFACE_POOL_INVITATION_OFFSET), 0);
}

#[test]
fn missing_gui_dvm_is_unavailable_not_a_fallback_provider() {
    let _serial = TRANSPORT_TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    assert_eq!(
        try_publish_full(core::ptr::null(), 1, 1, 4),
        DvmPresentOutcome::Unavailable
    );
}
