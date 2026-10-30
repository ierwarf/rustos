use alloc::vec;

use super::{
    ConsoleStreamKind, FD_CLOEXEC, HandleEntry, HandleTable, KernelHandle, MAX_DYNAMIC_FD,
    VfsDirectoryHandle, max_dynamic_entries,
};
use crate::memory::paging::UserRegion;
use crate::user::linux as linux_abi;
use kernel_object::api::{
    handle::{FileHandleRights, HandleOwner, HandleRights, JobHandleRights},
    identity::{ObjectKind, ObjectOwner},
};
use x86_64::VirtAddr;

#[test]
fn job_rights_wire_bits_match_the_typed_kernel_rights() {
    use rustos_user_abi::syscall::*;
    for (wire, typed) in [
        (JOB_RIGHT_INSPECT, JobHandleRights::INSPECT), (JOB_RIGHT_WAIT, JobHandleRights::WAIT),
        (JOB_RIGHT_CREATE_JOB, JobHandleRights::CREATE_JOB), (JOB_RIGHT_CREATE_PROCESS, JobHandleRights::CREATE_PROCESS),
        (JOB_RIGHT_SET_LIMITS, JobHandleRights::SET_LIMITS), (JOB_RIGHT_SET_POLICY, JobHandleRights::SET_POLICY),
        (JOB_RIGHT_KILL, JobHandleRights::KILL), (JOB_RIGHT_SET_CRITICAL, JobHandleRights::SET_CRITICAL),
        (JOB_RIGHT_DUPLICATE, JobHandleRights::DUPLICATE), (JOB_RIGHT_TRANSFER, JobHandleRights::TRANSFER),
    ] { assert_eq!(wire, typed.bits()); }
}

#[test]
fn a_raw_job_identity_never_recreates_descriptor_authority() {
    let mut table = HandleTable::new();
    let fd = table.install(KernelHandle::Job(crate::multitask::ROOT_JOB)).unwrap();
    let entry = table.get_entry(fd).unwrap();
    assert_eq!(entry.rights(), HandleRights::Job(JobHandleRights::empty()));
    assert_eq!(entry.token().owner(), HandleOwner::Job);
    assert_eq!(entry.token().identity().unwrap().kind(), ObjectKind::Job);
    assert!(table.duplicate_for_transfer(fd).is_none());
    assert_eq!(table.duplicate_min(fd, 0, false), None);
}

#[test]
fn two_live_job_publications_keep_independent_reservations() {
    // Publication windows overlap, but each table mutation is serialized by
    // the process-state lock. Model both reverse commit and cancellation.
    for cancel_first in [false, true] {
        let mut table = HandleTable::new();
        let (first, a) = table.reserve_slots(1).unwrap();
        let (second, b) = table.reserve_slots(1).unwrap();
        assert_ne!(a[0], b[0]);
        assert_eq!(table.reserved.get(&a[0]), Some(&first), "a later reserve must not steal an invisible slot");
        assert_eq!(table.reserved.get(&b[0]), Some(&second));
        let job = crate::multitask::ROOT_JOB;
        assert!(table.commit_reserved_job(second, b[0], job, JobHandleRights::INSPECT));
        if cancel_first {
            table.cancel_reservations(first, &a);
            assert!(table.get_entry(a[0]).is_none());
        } else {
            assert!(table.commit_reserved_job(first, a[0], job, JobHandleRights::CREATE_PROCESS));
            assert_eq!(table.get_entry(a[0]).unwrap().rights(), HandleRights::Job(JobHandleRights::CREATE_PROCESS));
        }
        assert_eq!(table.get_entry(b[0]).unwrap().rights(), HandleRights::Job(JobHandleRights::INSPECT));
        assert!(table.reserved.is_empty(), "no unreachable reservation may remain");
    }
}

#[test]
fn a_job_descriptor_publishes_only_through_its_exact_live_reservation() {
    let mut table = HandleTable::new();
    let job = crate::multitask::ROOT_JOB;
    let (old, slots) = table.reserve_slots(1).unwrap();
    let fd = slots[0];
    assert!(table.get_entry(fd).is_none());
    table.cancel_reservations(old, &slots);
    let (current, reused) = table.reserve_slots(1).unwrap();
    assert_eq!(reused, slots);
    assert_ne!(current, old);
    table.cancel_reservations(old, &slots);
    assert!(!table.commit_reserved_job(old, fd, job, JobHandleRights::ALL));
    assert!(!table.commit_reserved_job(current, fd, crate::multitask::JobId::from_raw(0), JobHandleRights::ALL));
    assert!(table.get_entry(fd).is_none());
    assert!(table.commit_reserved_job(current, fd, job, JobHandleRights::INSPECT));
    assert!(!table.commit_reserved_job(current, fd, job, JobHandleRights::ALL));
    assert_eq!(table.get_entry(fd).unwrap().rights(), HandleRights::Job(JobHandleRights::INSPECT));
}

#[test]
fn explicit_job_duplicates_require_duplicate_and_preserve_attenuation() {
    let mut table = HandleTable::new();
    let handle = KernelHandle::Job(crate::multitask::ROOT_JOB);
    let inspect = HandleRights::Job(JobHandleRights::INSPECT);
    let fd = table.install_with_open_flags_and_rights(handle.clone(), 0, inspect).unwrap();
    let target = table.install(handle.clone()).unwrap();
    assert_eq!(table.duplicate_min(fd, 0, false), None);
    assert!(table.duplicate_exact_with_replaced(fd, target, false).is_none());
    assert_eq!(table.get_entry(target).unwrap().rights(), HandleRights::Job(JobHandleRights::empty()));
    // A no-op dup2 does not create another capability.
    assert_eq!(table.duplicate_exact(fd, fd, false), Some(fd));
    let rights = HandleRights::Job(JobHandleRights::INSPECT.union(JobHandleRights::DUPLICATE));
    let source = table.install_with_open_flags_and_rights(handle, 0, rights).unwrap();
    let copy = table.duplicate_min(source, 0, true).unwrap();
    assert_eq!(table.get_entry(copy).unwrap().rights(), rights);
    assert_eq!(table.get_entry(copy).unwrap().fd_flags(), FD_CLOEXEC);
    assert_eq!(table.duplicate_exact(source, target, false), Some(target));
    assert_eq!(table.get_entry(target).unwrap().rights(), rights);
    assert!(table.duplicate_for_transfer(source).is_none());
}

#[test]
fn job_transfer_and_fork_preserve_exact_rights_and_generation() {
    let mut source = HandleTable::new();
    let rights = HandleRights::Job(JobHandleRights::CREATE_PROCESS.union(JobHandleRights::TRANSFER));
    let job = crate::multitask::JobId::from_raw((7_u64 << 32) | 9);
    let fd = source.install_with_open_flags_and_rights(KernelHandle::Job(job), 0, rights).unwrap();
    let token = source.get_entry(fd).unwrap().token();
    assert_eq!(token.object_id(), job.to_raw());
    let identity = token.identity().unwrap();
    assert_eq!((identity.slot(), identity.generation()), (8, 9));
    let transferred = source.duplicate_for_transfer(fd).unwrap();
    let mut destination = HandleTable::new();
    let received = destination.install_transferred(transferred).unwrap();
    assert_eq!(destination.get_entry(received).unwrap().rights(), rights);
    assert_eq!(destination.get_entry(received).unwrap().token(), token);
    assert!(matches!(destination.get(received), Some(KernelHandle::Job(id)) if *id == job));
    // Fork inherits the existing table, not an explicit DUPLICATE request.
    let inherited = source.clone();
    assert_eq!(inherited.get_entry(fd).unwrap().rights(), rights);
    assert_eq!(inherited.get_entry(fd).unwrap().token(), token);
    let raw = destination.close(received).unwrap();
    let rewrapped = destination.install(raw).unwrap();
    assert_eq!(destination.get_entry(rewrapped).unwrap().rights(), HandleRights::Job(JobHandleRights::empty()));
}

#[test]
fn install_entry_min_keeps_existing_dynamic_fds() {
    let mut table = HandleTable::new();

    let fd0 = table.install(KernelHandle::VfsDirectory(VfsDirectoryHandle::new(
        "/a".into(),
        vec![],
    )));
    let fd1 = table.install(KernelHandle::VfsDirectory(VfsDirectoryHandle::new(
        "/b".into(),
        vec![],
    )));
    let fd2 = table.install(KernelHandle::VfsDirectory(VfsDirectoryHandle::new(
        "/c".into(),
        vec![],
    )));

    assert_eq!(fd0, Some(3));
    assert_eq!(fd1, Some(4));
    assert_eq!(fd2, Some(5));
}

#[test]
fn standard_descriptors_are_real_unique_open_descriptions() {
    let table = HandleTable::new();
    let stdin = table.get(0).expect("stdin");
    let stdout = table.get(1).expect("stdout");
    let stderr = table.get(2).expect("stderr");
    assert_eq!(stdin.console_stream(), Some(ConsoleStreamKind::Input));
    assert_eq!(stdout.console_stream(), Some(ConsoleStreamKind::Output));
    assert_eq!(stderr.console_stream(), Some(ConsoleStreamKind::Error));
    assert_ne!(
        table.get_entry(0).expect("stdin entry").token(),
        table.get_entry(1).expect("stdout entry").token()
    );
    assert_ne!(
        table.get_entry(1).expect("stdout entry").token(),
        table.get_entry(2).expect("stderr entry").token()
    );
}

#[test]
fn nonreusable_console_descriptors_carry_the_open_description_identity_adapter() {
    let table = HandleTable::new();
    for fd in 0..3 {
        let token = table.get_entry(fd).expect("standard descriptor").token();
        let identity = token.identity().expect("console token adapter");
        assert_eq!(identity.owner(), ObjectOwner::Ps);
        assert_eq!(identity.kind(), ObjectKind::OpenDescription);
        assert_eq!(identity.slot(), token.object_id());
        assert_eq!(identity.generation(), 1);
    }
}

#[test]
fn close_and_dup_reuse_standard_slots_with_one_open_description() {
    let mut table = HandleTable::new();
    let stdin_token = table.get_entry(0).expect("stdin").token();
    let stdin_description_token = match table.get(0).expect("stdin") {
        KernelHandle::Console(console) => console.token_id(),
        _ => panic!("stdin must be a console"),
    };
    let closed = table.close(1).expect("close stdout");
    assert_eq!(closed.console_stream(), Some(ConsoleStreamKind::Output));

    assert_eq!(table.duplicate_min(0, 0, false), Some(1));
    assert_eq!(
        table.get_entry(1).expect("duplicated stdin").token(),
        stdin_token
    );

    let mut child = table.clone();
    assert_eq!(
        child.get_entry(0).expect("child stdin").token(),
        stdin_token
    );
    assert_eq!(
        child.get_entry(1).expect("child duplicated stdin").token(),
        stdin_token
    );

    let _ = table.close(0).expect("parent stdin");
    let _ = table.close(1).expect("parent duplicate");
    assert!(super::ConsoleHandle::token_is_live(stdin_description_token));
    let _ = child.close(0).expect("child stdin");
    assert!(super::ConsoleHandle::token_is_live(stdin_description_token));
    let final_ref = match child.close(1).expect("child duplicate") {
        KernelHandle::Console(console) => console,
        _ => panic!("child duplicate must be a console"),
    };
    assert!(final_ref.is_last_reference());
    assert!(!super::ConsoleHandle::token_is_live(
        stdin_description_token
    ));
}

#[test]
fn console_last_close_ignores_transient_handle_snapshot() {
    let mut table = HandleTable::new();
    let snapshot = match table.get(0).expect("stdin").clone() {
        KernelHandle::Console(console) => console,
        _ => panic!("stdin must be a console"),
    };
    let token = snapshot.token_id();

    let closed = match table.close(0).expect("close stdin") {
        KernelHandle::Console(console) => console,
        _ => panic!("closed stdin must be a console"),
    };
    assert!(closed.is_last_reference());
    assert!(!super::ConsoleHandle::token_is_live(token));
    assert_eq!(super::ConsoleHandle::stream_for_token(token), None);

    drop(snapshot);
    drop(closed);
}

#[test]
fn close_cloexec_removes_only_flagged_entries() {
    let mut table = HandleTable::new();

    let keep_fd = table.install_entry(HandleEntry::new(
        KernelHandle::VfsDirectory(VfsDirectoryHandle::new("/keep".into(), vec![])),
        0,
        0,
    ));
    let drop_fd = table.install_entry(HandleEntry::new(
        KernelHandle::VfsDirectory(VfsDirectoryHandle::new("/drop".into(), vec![])),
        FD_CLOEXEC,
        0,
    ));

    let closed = table.close_cloexec();

    assert!(table.get(keep_fd.expect("keep descriptor")).is_some());
    assert!(table.get(drop_fd.expect("cloexec descriptor")).is_none());
    assert_eq!(closed.len(), 1);
    assert!(matches!(
        &closed[0],
        KernelHandle::VfsDirectory(directory) if directory.path() == "/drop"
    ));
}

#[test]
fn lifecycle_snapshot_is_descriptor_exact_and_filters_cloexec() {
    let mut table = HandleTable::new();
    let keep_fd = table
        .install_entry(HandleEntry::new(
            KernelHandle::VfsDirectory(VfsDirectoryHandle::new("/keep".into(), vec![])),
            0,
            0,
        ))
        .unwrap();
    let cloexec_fd = table
        .install_entry(HandleEntry::new(
            KernelHandle::VfsDirectory(VfsDirectoryHandle::new("/exec".into(), vec![])),
            FD_CLOEXEC,
            0,
        ))
        .unwrap();

    let all = table.entries_snapshot(false);
    assert_eq!(
        all.iter()
            .map(|(fd, _)| *fd)
            .collect::<alloc::vec::Vec<_>>(),
        vec![0, 1, 2, keep_fd, cloexec_fd]
    );
    let cloexec = table.entries_snapshot(true);
    assert_eq!(cloexec.len(), 1);
    assert_eq!(cloexec[0].0, cloexec_fd);
}

#[test]
fn receive_reservations_are_invisible_and_publish_atomically() {
    let mut table = HandleTable::new();
    let _ = table.close(0).expect("free standard descriptor");
    let (reservation_id, slots) = table.reserve_slots(2).expect("reserve receive slots");
    assert_eq!(slots, vec![0, 3]);
    assert!(table.is_reserved(0));
    assert!(table.is_reserved(3));
    assert!(table.get_entry(0).is_none());
    assert!(table.get_entry(3).is_none());
    assert!(table.duplicate_exact(1, 3, false).is_none());
    assert!(
        table
            .replace_entry(
                3,
                Some(HandleEntry::new(
                    KernelHandle::VfsDirectory(VfsDirectoryHandle::new(
                        "/replacement".into(),
                        vec![],
                    )),
                    0,
                    0,
                )),
            )
            .is_none()
    );

    let mut child = table.clone();
    assert_eq!(
        child.install(KernelHandle::VfsDirectory(VfsDirectoryHandle::new(
            "/child".into(),
            vec![],
        ))),
        Some(0),
        "fork must not inherit a parent's in-flight receive transaction"
    );

    let unrelated = table
        .install(KernelHandle::VfsDirectory(VfsDirectoryHandle::new(
            "/unrelated".into(),
            vec![],
        )))
        .expect("ordinary install must skip reservations");
    assert_eq!(unrelated, 4);

    let entries = ["/first", "/second"]
        .into_iter()
        .map(|path| {
            super::TransferredHandleEntry::from_entry(HandleEntry::new(
                KernelHandle::VfsDirectory(VfsDirectoryHandle::new(path.into(), vec![])),
                0,
                0,
            ))
            .expect("transferable directory")
        })
        .collect();
    table
        .commit_reserved_transfers(reservation_id, &slots, entries)
        .expect("commit reservations");
    assert!(table.get_entry(0).is_some());
    assert!(table.get_entry(3).is_some());
    assert!(!table.is_reserved(0));
    assert!(!table.is_reserved(3));
}

#[test]
fn handle_fault_boundaries_preserve_reservation_atomicity() {
    let mut table = HandleTable::new();
    assert!(
        table.reserve_slots_faultable(1, true).is_none(),
        "injected reserve failure must not allocate a reservation"
    );
    assert!(!table.is_reserved(3));

    let (reservation_id, slots) = table.reserve_slots(1).expect("reserve slot");
    let transferred = super::TransferredHandleEntry::from_entry(HandleEntry::new(
        KernelHandle::VfsDirectory(VfsDirectoryHandle::new("/faultable".into(), vec![])),
        0,
        0,
    ))
    .expect("transferable directory");
    let entries = table
        .commit_reserved_transfers_faultable(reservation_id, &slots, vec![transferred], true)
        .expect_err("injected commit failure");
    assert_eq!(entries.len(), 1);
    assert!(table.get_entry(slots[0]).is_none());
    assert!(table.is_reserved(slots[0]));

    table
        .commit_reserved_transfers(reservation_id, &slots, entries)
        .expect("retry exact reservation");
    assert!(table.get_entry(slots[0]).is_some());
    assert!(!table.is_reserved(slots[0]));
}

#[test]
fn cancelled_receive_reservation_is_reusable() {
    let mut table = HandleTable::new();
    let (reservation_id, slots) = table.reserve_slots(1).expect("reserve receive slot");
    assert_eq!(slots, vec![3]);
    table.cancel_reservations(reservation_id, &slots);
    assert_eq!(
        table.install(KernelHandle::VfsDirectory(VfsDirectoryHandle::new(
            "/reuse".into(),
            vec![],
        ))),
        Some(3)
    );
}

#[test]
fn stale_reservation_cannot_cancel_or_commit_after_exec_boundary() {
    let mut table = HandleTable::new();
    let _ = table.close(0).expect("free standard descriptor");
    let (stale_id, stale_slots) = table.reserve_slots(1).expect("old reservation");
    assert_eq!(stale_slots, vec![0]);

    let _closed = table.close_all();
    let (live_id, live_slots) = table.reserve_slots(1).expect("new reservation");
    assert_eq!(live_slots, vec![0]);
    assert_ne!(stale_id, live_id);
    table.cancel_reservations(stale_id, &stale_slots);

    let entries = vec![
        super::TransferredHandleEntry::from_entry(HandleEntry::new(
            KernelHandle::VfsDirectory(VfsDirectoryHandle::new("/received".into(), vec![])),
            0,
            0,
        ))
        .expect("transferable directory"),
    ];
    let entries = table
        .commit_reserved_transfers(stale_id, &stale_slots, entries)
        .expect_err("stale transaction must not commit");
    table
        .commit_reserved_transfers(live_id, &live_slots, entries)
        .expect("live transaction commits");
    assert!(table.get_entry(0).is_some());
}

#[test]
fn set_status_flags_preserves_access_mode_and_masks_unknown_bits() {
    let mut entry = HandleEntry::new(
        KernelHandle::VfsDirectory(VfsDirectoryHandle::new("/flags".into(), vec![])),
        0,
        linux_abi::O_RDWR | linux_abi::O_APPEND,
    );

    entry.set_status_flags(linux_abi::O_RDONLY | linux_abi::O_NONBLOCK | (1_u64 << 63));
    assert_eq!(
        entry.status_flags() & linux_abi::O_ACCMODE,
        linux_abi::O_RDWR
    );
    assert_ne!(entry.status_flags() & linux_abi::O_NONBLOCK, 0);
    assert_eq!(entry.status_flags() & linux_abi::O_APPEND, 0);
    assert_eq!(entry.status_flags() & (1_u64 << 63), 0);
}

#[test]
fn duplicate_exact_replaces_target_and_applies_cloexec_flag() {
    let mut table = HandleTable::new();
    let source_fd = table.install_entry(HandleEntry::new(
        KernelHandle::VfsDirectory(VfsDirectoryHandle::new("/source".into(), vec![])),
        0,
        linux_abi::O_RDONLY,
    ));
    let target_fd = table.install_entry(HandleEntry::new(
        KernelHandle::VfsDirectory(VfsDirectoryHandle::new("/target".into(), vec![])),
        0,
        linux_abi::O_RDONLY,
    ));

    let (duplicated_fd, retired) = table
        .duplicate_exact_with_replaced(
            source_fd.expect("source descriptor"),
            target_fd.expect("target descriptor"),
            true,
        )
        .expect("dup2-style replace");
    assert_eq!(Some(duplicated_fd), target_fd);
    match retired.expect("exact target must be returned") {
        KernelHandle::VfsDirectory(dir) => assert_eq!(dir.path(), "/target"),
        other => panic!("expected retired VfsDirectory, got {other:?}"),
    }
    let replaced = table
        .get_entry(target_fd.expect("target descriptor"))
        .expect("duplicated entry");
    assert_eq!(replaced.fd_flags() & FD_CLOEXEC, FD_CLOEXEC);
    match replaced.handle() {
        KernelHandle::VfsDirectory(dir) => assert_eq!(dir.path(), "/source"),
        other => panic!("expected VfsDirectory after dup2-style replace, got {other:?}"),
    }
}

#[test]
fn duplicate_exact_preserves_handle_rights() {
    let mut table = HandleTable::new();
    let rights = HandleRights::File(FileHandleRights::READ);
    let source_fd = table.install_entry(HandleEntry::new_with_rights(
        KernelHandle::VfsDirectory(VfsDirectoryHandle::new("/source".into(), vec![])),
        rights,
        0,
        linux_abi::O_RDONLY,
    ));

    let target_fd = table
        .duplicate_exact(source_fd.expect("source descriptor"), 10, false)
        .expect("dup");

    assert_eq!(table.get_entry(target_fd).expect("target").rights(), rights);
}

#[test]
fn duplication_rejects_sparse_descriptor_indices_above_the_ceiling() {
    let mut table = HandleTable::new();
    let source_fd = table.install(KernelHandle::VfsDirectory(VfsDirectoryHandle::new(
        "/source".into(),
        vec![],
    )));

    assert_eq!(
        table.duplicate_exact(
            source_fd.expect("source descriptor"),
            MAX_DYNAMIC_FD + 1,
            false,
        ),
        None
    );
    assert_eq!(
        table.duplicate_min(
            source_fd.expect("source descriptor"),
            MAX_DYNAMIC_FD + 1,
            false,
        ),
        None
    );
    assert_eq!(table.entries.len(), 1);
}

#[test]
fn transfer_duplicate_requires_transfer_right() {
    let mut table = HandleTable::new();
    let source_fd = table.install_entry(HandleEntry::new_with_rights(
        KernelHandle::VfsDirectory(VfsDirectoryHandle::new("/source".into(), vec![])),
        HandleRights::File(FileHandleRights::READ),
        0,
        linux_abi::O_RDONLY,
    ));

    assert!(
        table
            .duplicate_for_transfer(source_fd.expect("source descriptor"))
            .is_none()
    );
}

#[test]
fn transfer_install_preserves_rights_and_flags() {
    let mut source = HandleTable::new();
    let source_fd = source.install_entry(HandleEntry::new(
        KernelHandle::VfsDirectory(VfsDirectoryHandle::new("/source".into(), vec![])),
        FD_CLOEXEC,
        linux_abi::O_RDONLY | linux_abi::O_NONBLOCK,
    ));
    let transferred = source
        .duplicate_for_transfer(source_fd.expect("source descriptor"))
        .expect("transferable source fd");
    assert!(transferred.ipc_descriptor(0).is_none());
    let descriptor = transferred
        .ipc_descriptor(99)
        .expect("ipc transfer descriptor");
    assert_eq!(descriptor.transfer_id(), 99);
    assert_eq!(descriptor.token().owner(), HandleOwner::Io);
    assert!(descriptor.rights().allows_transfer());

    let mut target = HandleTable::new();
    let target_fd = target
        .install_transferred(transferred)
        .expect("target descriptor");
    let target_entry = target.get_entry(target_fd).expect("target fd");
    assert_eq!(target_entry.fd_flags() & FD_CLOEXEC, FD_CLOEXEC);
    assert_ne!(target_entry.status_flags() & linux_abi::O_NONBLOCK, 0);
    assert!(target_entry.rights().allows_transfer());
}

#[test]
fn directory_fds_are_file_caps_for_vfs_transfer() {
    let mut table = HandleTable::new();
    let dir_fd = table.install(KernelHandle::VfsDirectory(VfsDirectoryHandle::new(
        "/dir".into(),
        vec![],
    )));

    let transferred = table
        .duplicate_for_transfer(dir_fd.expect("directory descriptor"))
        .expect("directory fd should be transferable");
    assert!(transferred.entry().rights().allows_transfer());
}

#[test]
fn device_fds_are_transferable_for_policy_brokers() {
    let mut table = HandleTable::new();
    let display_fd = table.install(KernelHandle::Device(
        crate::io::device::DeviceHandle::with_access(
            kernel_object::api::device::DeviceId::Display,
            crate::io::device::DeviceAccessKind::Native,
        ),
    ));

    let transferred = table
        .duplicate_for_transfer(display_fd.expect("device descriptor"))
        .expect("device fd should be transferable after policy approval");
    assert!(transferred.entry().rights().allows_transfer());
}

#[test]
fn display_surface_count_ignores_other_handle_kinds() {
    let mut table = HandleTable::new();
    assert_eq!(table.display_surface_count(), 0);

    table.install(KernelHandle::VfsDirectory(VfsDirectoryHandle::new(
        "/file".into(),
        vec![],
    )));
    assert_eq!(table.display_surface_count(), 0);

    let surface = super::DisplaySurfaceHandle::new(
        16,
        16,
        crate::user::abi::device::PIXEL_FORMAT_BGRA8888,
        1,
    )
    .expect("surface");
    table.install(KernelHandle::DisplaySurface(surface));
    assert_eq!(table.display_surface_count(), 1);
}

#[test]
fn surface_overlap_segments_return_intersection_ranges() {
    let mut table = HandleTable::new();
    let mut surface = super::DisplaySurfaceHandle::new(
        1280,
        800,
        crate::user::abi::device::PIXEL_FORMAT_BGRA8888,
        1,
    )
    .expect("surface");
    surface.set_mapped_region(UserRegion {
        start: VirtAddr::new(0x4000_0000),
        page_count: 4,
    });
    table.install(KernelHandle::DisplaySurface(surface));

    let segments = table.surface_overlap_segments(0x4000_1000, 0x4000_5000);
    assert_eq!(segments, vec![(0x4000_1000, 0x3000)]);

    let disjoint = table.surface_overlap_segments(0x5000_0000, 0x5000_1000);
    assert!(disjoint.is_empty());
}

#[test]
fn dynamic_install_never_exceeds_descriptor_ceiling() {
    let mut table = HandleTable::new();
    let occupied = HandleEntry::new(
        KernelHandle::VfsDirectory(VfsDirectoryHandle::new("/occupied".into(), vec![])),
        0,
        linux_abi::O_RDONLY,
    );
    table.entries.resize(max_dynamic_entries(), Some(occupied));
    table.entries[max_dynamic_entries() - 1] = None;

    let last = table.install_entry_min(
        HandleEntry::new(
            KernelHandle::VfsDirectory(VfsDirectoryHandle::new("/last".into(), vec![])),
            0,
            linux_abi::O_RDONLY,
        ),
        MAX_DYNAMIC_FD,
    );
    assert_eq!(last, Some(MAX_DYNAMIC_FD));
    assert_eq!(table.entries.len(), max_dynamic_entries());
    assert!(!table.can_install_additional(1));

    let rejected = table.install(KernelHandle::VfsDirectory(VfsDirectoryHandle::new(
        "/overflow".into(),
        vec![],
    )));
    assert_eq!(rejected, None);
    assert_eq!(table.entries.len(), max_dynamic_entries());
}
