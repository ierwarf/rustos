mod receiver_waiter_tests;

use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use super::{
    ConsoleStreamKind, IpcError, IpcHeader, KernelHandle, accept_channel,
    acquire_shared_region_mapping, connect_named_port, connect_port, create_channel_pair,
    create_event, create_named_port, create_shared_region, dequeue_message,
    dequeue_message_with_limits, enqueue_message, event_signal_count, lookup_named_port,
    map_shared_region, port_name, queue_channel_for_accept, recv_endpoint,
    recv_endpoint_with_limits, recv_endpoint_with_limits_and_handles, release_shared_region,
    service_deferred_shared_region_reclaims, shared_region_len, signal_event,
};
use kernel_object::api::handle::{FileHandleRights, HandleOwner, HandleRights, HandleToken};
use kernel_object::api::identity::{ObjectKind, ObjectOwner};
use spin::Mutex;

static IPC_TEST_GUARD: Mutex<()> = Mutex::new(());

fn with_isolated_ipc_test(f: impl FnOnce()) {
    let _guard = IPC_TEST_GUARD.lock();
    super::with_ipc_objects(|objects| *objects = super::IpcObjectTable::new());
    super::ENDPOINTS.clear();
    super::ENDPOINT_QUOTAS.lock().clear();
    super::ENDPOINT_MESSAGES.clear();
    super::REPLIES.clear();
    super::SHARED_REGIONS.clear();
    super::SHARED_REGION_RECLAIMS.lock().clear();
    super::SHARED_REGION_ADMITTED.store(0, Ordering::Release);
    super::SHARED_REGION_BYTES_ADMITTED.store(0, Ordering::Release);
    super::SHARED_REGION_QUOTAS.lock().clear();
    super::set_endpoint_enqueue_binding_fault(None);
    super::set_endpoint_cancel_reply_binding_fault(false);
    super::set_endpoint_recv_stale_head_fault(false);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    super::with_ipc_objects(|objects| *objects = super::IpcObjectTable::new());
    super::ENDPOINTS.clear();
    super::ENDPOINT_QUOTAS.lock().clear();
    super::ENDPOINT_MESSAGES.clear();
    super::REPLIES.clear();
    super::SHARED_REGIONS.clear();
    super::SHARED_REGION_RECLAIMS.lock().clear();
    super::SHARED_REGION_ADMITTED.store(0, Ordering::Release);
    super::SHARED_REGION_BYTES_ADMITTED.store(0, Ordering::Release);
    super::SHARED_REGION_QUOTAS.lock().clear();
    super::set_endpoint_enqueue_binding_fault(None);
    super::set_endpoint_cancel_reply_binding_fault(false);
    super::set_endpoint_recv_stale_head_fault(false);
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

fn transferable_file_handle(id: u64) -> super::KernelTransferredHandle {
    super::KernelTransferredHandle::new(
        id,
        HandleToken::new(HandleOwner::Io, id),
        HandleRights::File(FileHandleRights::READ.union(FileHandleRights::TRANSFER)),
    )
}

fn non_transferable_file_handle(id: u64) -> super::KernelTransferredHandle {
    super::KernelTransferredHandle::new(
        id,
        HandleToken::new(HandleOwner::Io, id),
        HandleRights::File(FileHandleRights::READ),
    )
}

#[test]
fn kernel_transfer_ticket_binds_the_nonzero_transfer_object_generation() {
    assert!(super::KernelTransferTicket::new(0, 7, 9).is_none());
    assert!(super::KernelTransferTicket::new(7, 0, 9).is_none());
    assert!(super::KernelTransferTicket::new(7, 9, 0).is_none());

    let ticket = super::KernelTransferTicket::new(7, 9, 11).expect("valid ticket");
    let identity = ticket.identity();
    assert_eq!(identity.owner(), ObjectOwner::Ipc);
    assert_eq!(identity.kind(), ObjectKind::Transfer);
    assert_eq!(identity.slot(), ticket.transfer_id());
    assert_eq!(identity.generation(), ticket.batch_generation());
}

#[test]
fn endpoint_and_reply_handles_decode_only_in_range_generational_identities() {
    let endpoint = super::KernelEndpointHandle::from_raw((7_u64 << 16) | 3);
    let endpoint_identity = endpoint.identity().expect("endpoint identity");
    assert_eq!(endpoint_identity.owner(), ObjectOwner::Ipc);
    assert_eq!(endpoint_identity.kind(), ObjectKind::Endpoint);
    assert_eq!(endpoint_identity.slot(), 3);
    assert_eq!(endpoint_identity.generation(), 7);

    let reply = super::KernelReplyHandle::from_raw((9_u64 << 16) | 11);
    let reply_identity = reply.identity().expect("reply identity");
    assert_eq!(reply_identity.owner(), ObjectOwner::Ipc);
    assert_eq!(reply_identity.kind(), ObjectKind::Reply);
    assert_eq!(reply_identity.slot(), 11);
    assert_eq!(reply_identity.generation(), 9);

    assert!(
        super::KernelEndpointHandle::from_raw(3)
            .identity()
            .is_none()
    );
    assert!(
        super::KernelEndpointHandle::from_raw((1_u64 << 16) | 513)
            .identity()
            .is_none()
    );
    assert!(
        super::KernelReplyHandle::from_raw((1_u64 << 16) | 129)
            .identity()
            .is_none()
    );
}

#[test]
fn transferred_handle_derivation_only_attenuates_typed_rights() {
    let parent_rights = FileHandleRights::READ
        .union(FileHandleRights::WRITE)
        .union(FileHandleRights::TRANSFER);
    let parent = super::KernelTransferredHandle::new(
        7,
        HandleToken::new(HandleOwner::Io, 9),
        HandleRights::File(parent_rights),
    );

    let child = parent
        .attenuate(HandleRights::File(
            FileHandleRights::READ.union(FileHandleRights::TRANSFER),
        ))
        .expect("read-only transfer derivation");
    assert_eq!(child.transfer_id(), parent.transfer_id());
    assert_eq!(child.token(), parent.token());
    assert!(child.rights().allows_read());
    assert!(!child.rights().allows_write());
    assert!(child.is_transferable());

    assert!(
        parent
            .attenuate(HandleRights::File(
                parent_rights.union(FileHandleRights::APPEND),
            ))
            .is_none()
    );
    assert!(
        parent
            .attenuate(HandleRights::Device(
                kernel_object::api::handle::DeviceHandleRights::READ,
            ))
            .is_none()
    );
}

#[test]
fn channel_messages_arrive_in_peer_queue_order() {
    with_isolated_ipc_test(|| {
        let (left, right) = create_channel_pair().expect("create channel pair");
        enqueue_message(
            left,
            IpcHeader {
                opcode: 1,
                ..IpcHeader::default()
            },
            b"hello",
            &[
                KernelHandle::Console(ConsoleStreamKind::Input),
                KernelHandle::Console(ConsoleStreamKind::Output),
            ],
        )
        .expect("enqueue first");
        enqueue_message(
            left,
            IpcHeader {
                opcode: 2,
                ..IpcHeader::default()
            },
            b"world",
            &[],
        )
        .expect("enqueue second");

        let first = dequeue_message(right)
            .expect("dequeue first")
            .expect("message present");
        let second = dequeue_message(right)
            .expect("dequeue second")
            .expect("message present");

        assert_eq!(first.header.opcode, 1);
        assert_eq!(first.payload, b"hello");
        assert_eq!(first.attached_handles.len(), 2);
        assert!(matches!(
            &first.attached_handles[0],
            KernelHandle::Console(ConsoleStreamKind::Input)
        ));
        assert!(matches!(
            &first.attached_handles[1],
            KernelHandle::Console(ConsoleStreamKind::Output)
        ));
        assert_eq!(second.header.opcode, 2);
        assert_eq!(second.payload, b"world");
    });
}

#[test]
fn ports_accept_queued_server_channels() {
    with_isolated_ipc_test(|| {
        let port = create_named_port(None).expect("create port");
        let (_client, server) = create_channel_pair().expect("create channel pair");
        queue_channel_for_accept(port, server).expect("queue server channel");
        let accepted = accept_channel(port)
            .expect("accept")
            .expect("accepted channel");
        assert_eq!(accepted, server);
    });
}

#[test]
fn events_and_shared_regions_track_basic_state() {
    with_isolated_ipc_test(|| {
        let event = create_event().expect("create event");
        assert_eq!(event_signal_count(event), Some(0));
        assert_eq!(signal_event(event), Ok(1));
        assert_eq!(event_signal_count(event), Some(1));

        let region = create_shared_region(8192).expect("create region");
        assert_eq!(shared_region_len(region), Some(8192));
        let (ptr, len) = map_shared_region(region).expect("map region");
        assert_eq!(len, 8192);
        assert!(!ptr.is_null());

        let mapping = acquire_shared_region_mapping(region).expect("retain mapping");
        let cloned_mapping = mapping.clone();
        release_shared_region(region);
        assert_eq!(shared_region_len(region), Some(8192));
        drop(mapping);
        assert_eq!(shared_region_len(region), Some(8192));
        drop(cloned_mapping);
        assert_eq!(shared_region_len(region), None);
        assert_eq!(service_deferred_shared_region_reclaims(64), 1);
    });
}

#[test]
fn process_shared_region_quota_is_bounded_until_reclaim_completes() {
    with_isolated_ipc_test(|| {
        let mut regions = [None; super::MAX_SHARED_REGIONS_PER_PROCESS];
        for slot in &mut regions {
            *slot = Some(
                super::create_shared_region_for_process(51, 1)
                    .expect("within process shared-region quota"),
            );
        }
        assert_eq!(
            super::create_shared_region_for_process(51, 1),
            Err(IpcError::NoMemory)
        );
        for region in regions.into_iter().flatten() {
            release_shared_region(region);
        }
        assert_eq!(
            super::create_shared_region_for_process(51, 1),
            Err(IpcError::NoMemory),
            "queued backing must remain charged until physical reclaim"
        );
        for _ in 0..super::MAX_SHARED_REGIONS_PER_PROCESS {
            assert_eq!(service_deferred_shared_region_reclaims(1), 1);
        }
        assert!(
            super::create_shared_region_for_process(51, 1).is_ok(),
            "completed reclaim must return process quota"
        );
    });
}

#[test]
fn named_ports_retain_port_name() {
    with_isolated_ipc_test(|| {
        let mut name = crate::ipc_core::PortName::empty();
        name.bytes[..4].copy_from_slice(b"test");
        name.len = 4;
        let port = create_named_port(Some(name)).expect("create named port");
        assert_eq!(port_name(port), Some(name));
        assert_eq!(lookup_named_port(name), Some(port));
    });
}

#[test]
fn connect_port_queues_server_channel_for_accept() {
    with_isolated_ipc_test(|| {
        let port = create_named_port(None).expect("create port");
        let client = connect_port(port).expect("connect port");
        let server = accept_channel(port)
            .expect("accept")
            .expect("server channel");
        enqueue_message(
            client,
            IpcHeader {
                opcode: 99,
                ..IpcHeader::default()
            },
            b"ping",
            &[],
        )
        .expect("enqueue");
        let received = dequeue_message(server).expect("dequeue").expect("message");
        assert_eq!(received.header.opcode, 99);
        assert_eq!(received.payload, b"ping");
    });
}

#[test]
fn connect_named_port_finds_registered_port() {
    with_isolated_ipc_test(|| {
        let name = crate::ipc_core::PortName::try_from_str("display-host").expect("port name");
        let port = create_named_port(Some(name)).expect("create named port");
        let client = connect_named_port(name).expect("connect named port");
        let server = accept_channel(port)
            .expect("accept")
            .expect("server channel");
        enqueue_message(
            client,
            IpcHeader {
                opcode: 7,
                ..IpcHeader::default()
            },
            b"surface",
            &[
                KernelHandle::Console(ConsoleStreamKind::Input),
                KernelHandle::Console(ConsoleStreamKind::Output),
                KernelHandle::Console(ConsoleStreamKind::Error),
            ],
        )
        .expect("enqueue");
        let received = dequeue_message(server).expect("dequeue").expect("message");
        assert_eq!(received.header.opcode, 7);
        assert_eq!(received.payload, b"surface");
        assert_eq!(received.attached_handles.len(), 3);
    });
}

#[test]
fn attached_handles_are_cloned_into_messages() {
    with_isolated_ipc_test(|| {
        let (left, right) = create_channel_pair().expect("create channel pair");
        enqueue_message(
            left,
            IpcHeader {
                opcode: 55,
                ..IpcHeader::default()
            },
            b"",
            &[
                KernelHandle::Console(ConsoleStreamKind::Input),
                KernelHandle::Console(ConsoleStreamKind::Output),
            ],
        )
        .expect("enqueue");
        let received = dequeue_message(right).expect("dequeue").expect("message");
        assert_eq!(received.attached_handles.len(), 2);
        assert!(matches!(
            &received.attached_handles[0],
            KernelHandle::Console(ConsoleStreamKind::Input)
        ));
        assert!(matches!(
            &received.attached_handles[1],
            KernelHandle::Console(ConsoleStreamKind::Output)
        ));
    });
}

#[test]
fn enqueue_message_normalizes_header_lengths() {
    with_isolated_ipc_test(|| {
        let (left, right) = create_channel_pair().expect("create channel pair");
        enqueue_message(
            left,
            IpcHeader {
                opcode: 9,
                reserved: u16::MAX,
                ..IpcHeader::default()
            },
            b"hello",
            &[KernelHandle::Console(ConsoleStreamKind::Input)],
        )
        .expect("enqueue");

        let received = dequeue_message(right)
            .expect("dequeue")
            .expect("message present");
        assert_eq!(received.header.payload_len, 5);
        assert_eq!(received.header.handle_count, 1);
        assert_eq!(received.header.reserved, 0);
    });
}

#[test]
fn duplicate_named_ports_are_rejected() {
    with_isolated_ipc_test(|| {
        let name = crate::ipc_core::PortName::try_from_str("display-host").expect("port name");
        create_named_port(Some(name)).expect("first named port");
        assert_eq!(
            create_named_port(Some(name)),
            Err(IpcError::InvalidArgument)
        );
    });
}

#[test]
fn duplicate_pending_channel_is_rejected() {
    with_isolated_ipc_test(|| {
        let port = create_named_port(None).expect("create port");
        let (_client, server) = create_channel_pair().expect("create channel pair");
        queue_channel_for_accept(port, server).expect("queue once");
        assert_eq!(
            queue_channel_for_accept(port, server),
            Err(IpcError::InvalidArgument)
        );
    });
}

#[test]
fn buffer_too_small_preserves_front_message() {
    with_isolated_ipc_test(|| {
        let (left, right) = create_channel_pair().expect("create channel pair");
        enqueue_message(
            left,
            IpcHeader {
                opcode: 77,
                ..IpcHeader::default()
            },
            b"hello",
            &[KernelHandle::Console(ConsoleStreamKind::Input)],
        )
        .expect("enqueue");

        assert!(matches!(
            dequeue_message_with_limits(right, 4, 1),
            Err(IpcError::BufferTooSmall)
        ));

        let message = dequeue_message(right)
            .expect("dequeue")
            .expect("message present");
        assert_eq!(message.payload, b"hello");
        assert_eq!(message.attached_handles.len(), 1);
    });
}

#[test]
fn endpoint_call_recv_reply_completes_response() {
    with_isolated_ipc_test(|| {
        let endpoint = super::create_endpoint().expect("create endpoint");
        let (reply, receiver) =
            super::enqueue_endpoint_call(endpoint, 41, b"statx").expect("enqueue call");
        assert_eq!(receiver, None);

        let (server_reply, request) = recv_endpoint(endpoint)
            .expect("recv endpoint")
            .expect("message queued");
        assert_eq!(server_reply, reply);
        assert_eq!(request, b"statx");

        let caller = super::complete_endpoint_reply(reply, b"ok").expect("reply");
        assert_eq!(caller, 41);
        let response = super::take_endpoint_response(reply)
            .expect("take response")
            .expect("response present");
        assert_eq!(response, b"ok");
        assert_eq!(
            super::take_endpoint_response(reply),
            Err(IpcError::InvalidHandle)
        );
    });
}

#[test]
fn endpoint_fault_boundaries_fail_before_queue_or_reply_mutation() {
    with_isolated_ipc_test(|| {
        let endpoint = super::create_endpoint().expect("create endpoint");
        assert_eq!(
            super::enqueue_endpoint_call_with_handles_faultable(
                endpoint,
                41,
                b"request",
                &[],
                super::EndpointCallPriority::Ordinary,
                true,
            ),
            Err(IpcError::NoMemory)
        );
        assert_eq!(recv_endpoint(endpoint), Ok(None));

        let (reply, _) =
            super::enqueue_endpoint_call(endpoint, 41, b"request").expect("enqueue call");
        let (server_reply, _) = recv_endpoint(endpoint)
            .expect("receive result")
            .expect("receive queued request");
        assert_eq!(server_reply, reply);
        assert_eq!(
            super::complete_endpoint_reply_with_handles_faultable(reply, b"response", &[], true,),
            Err(IpcError::NoMemory)
        );
        assert_eq!(super::take_endpoint_response(reply), Ok(None));
        assert_eq!(super::complete_endpoint_reply(reply, b"response"), Ok(41));
    });
}

#[test]
fn endpoint_enqueue_rejects_cross_endpoint_reply_binding() {
    with_isolated_ipc_test(|| {
        let endpoint = super::create_endpoint_for_task(Some(10)).expect("create endpoint");

        for (fault, transfer_id) in [
            (super::EndpointEnqueueBindingFault::EndpointId, 71),
            (super::EndpointEnqueueBindingFault::ReplyId, 72),
        ] {
            let handle = transferable_file_handle(transfer_id);
            super::set_endpoint_enqueue_binding_fault(Some(fault));
            assert_eq!(
                super::enqueue_endpoint_call_with_handles(endpoint, 41, b"request", &[handle]),
                Err(IpcError::NoMemory),
                "a mismatched {fault:?} must fail before publication"
            );
            assert_eq!(recv_endpoint(endpoint), Ok(None));

            // The failed call must leave the request transfer with its
            // caller so the same capability can be admitted exactly once
            // by a later, correctly bound request.
            super::set_endpoint_enqueue_binding_fault(None);
            let (reply, receiver) =
                super::enqueue_endpoint_call_with_handles(endpoint, 41, b"request", &[handle])
                    .expect("re-enqueue transfer after rejected binding");
            assert_eq!(receiver, Some(10));
            let (server_reply, request, received_handles) =
                recv_endpoint_with_limits_and_handles(endpoint, usize::MAX, 1)
                    .expect("receive admitted request")
                    .expect("request queued only after exact binding");
            assert_eq!(server_reply, reply);
            assert_eq!(request, b"request");
            assert_eq!(received_handles, alloc::vec![handle]);
            assert_eq!(super::complete_endpoint_reply(reply, b"ok"), Ok(41));
        }
    });
}

#[test]
fn owned_endpoint_rejects_foreign_receiver_and_reply_task() {
    with_isolated_ipc_test(|| {
        let endpoint = super::create_endpoint_for_task(Some(10)).expect("create endpoint");
        let (reply, _) =
            super::enqueue_endpoint_call(endpoint, 22, b"request").expect("enqueue call");

        assert_eq!(
            super::authorize_endpoint_receiver(endpoint, 11),
            Err(super::IpcError::PermissionDenied)
        );
        assert_eq!(super::authorize_endpoint_receiver(endpoint, 10), Ok(()));
        assert_eq!(
            super::complete_endpoint_reply_for_task(reply, 11, b"forged"),
            Err(super::IpcError::PermissionDenied)
        );
        assert_eq!(
            super::complete_endpoint_reply_for_task(reply, 10, b"ok"),
            Ok(22)
        );
    });
}

#[test]
fn process_owned_endpoint_allows_worker_and_rejects_foreign_process() {
    with_isolated_ipc_test(|| {
        let endpoint = super::create_endpoint_for_process(10).expect("create endpoint");
        let (reply, _) =
            super::enqueue_endpoint_call(endpoint, 22, b"request").expect("enqueue call");

        assert_eq!(
            super::authorize_endpoint_receiver_for_process(endpoint, 11),
            Err(super::IpcError::PermissionDenied)
        );
        // A different task in the owning service process may receive and
        // reply; this is how uiserver's display-policy worker operates.
        assert_eq!(
            super::authorize_endpoint_receiver_for_process(endpoint, 10),
            Ok(())
        );
        assert_eq!(
            super::complete_endpoint_reply_for_process(reply, 11, b"forged"),
            Err(super::IpcError::PermissionDenied)
        );
        assert_eq!(
            super::complete_endpoint_reply_for_process(reply, 10, b"ok"),
            Ok(22)
        );
    });
}

/// Authority confinement: the scheduler establishes bounded priority
/// inheritance for a process-owned endpoint by asking for the process
/// owner of a live reply. That must resolve to the exact owning process
/// for a process-owned endpoint and to nothing at all for a task-owned
/// or unowned one - never a fabricated identity that could grant
/// donation authority the caller never held.
#[test]
fn endpoint_receiver_process_for_reply_is_exact_and_never_fabricated() {
    with_isolated_ipc_test(|| {
        let process_endpoint = super::create_endpoint_for_process(30).expect("process endpoint");
        let (process_reply, _) = super::enqueue_endpoint_call(process_endpoint, 1, b"request")
            .expect("enqueue process call");
        assert_eq!(
            super::endpoint_receiver_process_for_reply(process_reply),
            Some(30)
        );

        let task_endpoint = super::create_endpoint_for_task(Some(31)).expect("task endpoint");
        let (task_reply, _) =
            super::enqueue_endpoint_call(task_endpoint, 2, b"request").expect("enqueue task call");
        assert_eq!(super::endpoint_receiver_process_for_reply(task_reply), None);

        let open_endpoint = super::create_endpoint().expect("unowned endpoint");
        let (open_reply, _) =
            super::enqueue_endpoint_call(open_endpoint, 3, b"request").expect("enqueue open call");
        assert_eq!(super::endpoint_receiver_process_for_reply(open_reply), None);
    });
}

#[test]
fn process_endpoint_quota_is_bounded_and_returned_on_exit() {
    with_isolated_ipc_test(|| {
        for _ in 0..super::MAX_ENDPOINTS_PER_PROCESS {
            super::create_endpoint_for_process(41).expect("within process endpoint quota");
        }
        assert_eq!(
            super::create_endpoint_for_process(41),
            Err(IpcError::NoMemory)
        );

        let _ = super::fail_endpoints_owned_by_process(41, IpcError::PeerClosed);
        for _ in 0..super::MAX_ENDPOINTS_PER_PROCESS {
            super::create_endpoint_for_process(41).expect("quota returned after process exit");
        }
    });
}

#[test]
fn endpoint_request_handles_require_explicit_receive_capacity() {
    with_isolated_ipc_test(|| {
        let endpoint = super::create_endpoint().expect("create endpoint");
        let handle = transferable_file_handle(11);
        let (reply, _) =
            super::enqueue_endpoint_call_with_handles(endpoint, 41, b"open", &[handle])
                .expect("enqueue call with handle");

        assert_eq!(
            recv_endpoint_with_limits(endpoint, usize::MAX),
            Err(IpcError::BufferTooSmall)
        );
        assert_eq!(recv_endpoint(endpoint), Ok(None));

        let cancelled = super::cancel_endpoint_call_with_transfers(reply, 41)
            .expect("caller reclaims a request that no receiver could accept");
        assert_eq!(
            cancelled.disposition,
            super::CancelledCallDisposition::InFlight
        );
        assert_eq!(cancelled.transfers, alloc::vec![handle]);
        assert_eq!(
            super::take_endpoint_response(reply),
            Err(IpcError::InvalidHandle)
        );
    });
}

#[test]
fn endpoint_rejects_non_transferable_request_handles() {
    with_isolated_ipc_test(|| {
        let endpoint = super::create_endpoint().expect("create endpoint");
        assert_eq!(
            super::enqueue_endpoint_call_with_handles(
                endpoint,
                41,
                b"open",
                &[non_transferable_file_handle(12)],
            ),
            Err(IpcError::InvalidArgument)
        );
        assert_eq!(
            super::enqueue_endpoint_call_with_handles(
                endpoint,
                41,
                b"open",
                &[super::KernelTransferredHandle::new(
                    0,
                    HandleToken::new(HandleOwner::Io, 12),
                    HandleRights::File(FileHandleRights::READ.union(FileHandleRights::TRANSFER),),
                )],
            ),
            Err(IpcError::InvalidArgument)
        );
    });
}

#[test]
fn endpoint_request_handle_limit_is_bounded() {
    with_isolated_ipc_test(|| {
        let endpoint = super::create_endpoint().expect("create endpoint");
        let mut handles = alloc::vec::Vec::new();
        for index in 0..=super::MAX_ENDPOINT_TRANSFER_HANDLES {
            handles.push(transferable_file_handle(index as u64 + 1));
        }

        assert_eq!(
            super::enqueue_endpoint_call_with_handles(endpoint, 41, b"open", &handles),
            Err(IpcError::InvalidArgument)
        );
    });
}

#[test]
fn endpoint_reply_handles_require_explicit_take_capacity() {
    with_isolated_ipc_test(|| {
        let endpoint = super::create_endpoint().expect("create endpoint");
        let (reply, _) =
            super::enqueue_endpoint_call(endpoint, 41, b"request").expect("enqueue call");
        let (_server_reply, _request) = recv_endpoint(endpoint)
            .expect("recv endpoint")
            .expect("message queued");
        let handle = transferable_file_handle(21);

        assert_eq!(
            super::complete_endpoint_reply_with_handles(reply, b"ok", &[handle]),
            Ok(41)
        );
        assert_eq!(
            super::take_endpoint_response(reply),
            Err(IpcError::BufferTooSmall)
        );

        let (bytes, handles) = super::take_endpoint_response_with_handle_limit(reply, 1)
            .expect("take response")
            .expect("response present");
        assert_eq!(bytes, b"ok");
        assert_eq!(handles, alloc::vec![handle]);
        assert_eq!(
            super::take_endpoint_response_with_handle_limit(reply, 1),
            Err(IpcError::InvalidHandle)
        );
    });
}

#[test]
fn prepared_reply_bind_rejects_foreign_and_duplicate_owner_without_mutation() {
    with_isolated_ipc_test(|| {
        let endpoint = super::create_endpoint_for_process(10).expect("create process endpoint");
        let (reply, _) =
            super::enqueue_endpoint_call(endpoint, 41, b"request").expect("enqueue call");
        let handle = transferable_file_handle(61);
        let foreign = transferable_file_handle(62);

        let rejected =
            super::bind_prepared_reply_handles_for_process(reply, 11, alloc::vec![foreign])
                .expect_err("foreign owner must not bind this reply");
        assert_eq!(rejected.error, IpcError::PermissionDenied);
        assert_eq!(rejected.handles, alloc::vec![foreign]);

        assert_eq!(
            super::bind_prepared_reply_handles_for_process(reply, 10, alloc::vec![handle]),
            Ok(())
        );
        let duplicate =
            super::bind_prepared_reply_handles_for_process(reply, 10, alloc::vec![foreign])
                .expect_err("one reply may own one prepared descriptor batch");
        assert_eq!(duplicate.error, IpcError::InvalidArgument);
        assert_eq!(duplicate.handles, alloc::vec![foreign]);

        assert_eq!(
            super::complete_endpoint_reply_for_process(reply, 10, b"ok"),
            Ok(41)
        );
        let (bytes, handles) = super::take_endpoint_response_with_handle_limit(reply, 1)
            .expect("take prepared response")
            .expect("completed response");
        assert_eq!(bytes, b"ok");
        assert_eq!(handles, alloc::vec![handle]);
        assert_eq!(
            super::take_endpoint_response_with_handle_limit(reply, 1),
            Err(IpcError::InvalidHandle)
        );
    });
}

#[test]
fn cancelling_before_reply_returns_prepared_descriptor_once() {
    with_isolated_ipc_test(|| {
        let endpoint = super::create_endpoint_for_process(10).expect("create process endpoint");
        let (reply, _) =
            super::enqueue_endpoint_call(endpoint, 41, b"request").expect("enqueue call");
        let handle = transferable_file_handle(63);
        assert_eq!(
            super::bind_prepared_reply_handles_for_process(reply, 10, alloc::vec![handle]),
            Ok(())
        );

        let cancelled =
            super::cancel_endpoint_call_with_transfers(reply, 41).expect("cancel prepared reply");
        assert_eq!(cancelled.transfers, alloc::vec![handle]);
        assert_eq!(
            super::complete_endpoint_reply_for_process(reply, 10, b"late"),
            Err(IpcError::InvalidHandle)
        );
    });
}

#[test]
fn endpoint_failure_returns_prepared_descriptor_in_error_cleanup() {
    with_isolated_ipc_test(|| {
        let endpoint = super::create_endpoint_for_process(10).expect("create process endpoint");
        let (reply, _) =
            super::enqueue_endpoint_call(endpoint, 41, b"request").expect("enqueue call");
        let handle = transferable_file_handle(64);
        assert_eq!(
            super::bind_prepared_reply_handles_for_process(reply, 10, alloc::vec![handle]),
            Ok(())
        );

        let wake_set = super::fail_endpoints_owned_by_process(10, IpcError::PeerClosed);
        assert_eq!(wake_set.callers(), &[41]);
        assert_eq!(
            super::take_endpoint_response_detailed(reply, 0),
            Ok(super::EndpointResponseTake::Error {
                error: IpcError::PeerClosed,
                discarded_request_handles: alloc::vec![handle],
            })
        );
    });
}

#[test]
fn malformed_reply_handles_do_not_consume_reply_cap() {
    with_isolated_ipc_test(|| {
        let endpoint = super::create_endpoint().expect("create endpoint");
        let (reply, _) =
            super::enqueue_endpoint_call(endpoint, 7, b"request").expect("enqueue call");
        let (_server_reply, _request) = recv_endpoint(endpoint)
            .expect("recv endpoint")
            .expect("message queued");

        assert_eq!(
            super::complete_endpoint_reply_with_handles(
                reply,
                b"bad",
                &[non_transferable_file_handle(33)],
            ),
            Err(IpcError::InvalidArgument)
        );
        assert_eq!(super::complete_endpoint_reply(reply, b"first"), Ok(7));
    });
}

#[test]
fn endpoint_reply_cap_is_one_shot() {
    with_isolated_ipc_test(|| {
        let endpoint = super::create_endpoint().expect("create endpoint");
        let (reply, _) =
            super::enqueue_endpoint_call(endpoint, 7, b"request").expect("enqueue call");
        assert_eq!(super::complete_endpoint_reply(reply, b"first"), Ok(7));
        assert_eq!(
            super::complete_endpoint_reply(reply, b"second"),
            Err(IpcError::InvalidArgument)
        );
    });
}

#[test]
fn endpoint_queue_limit_is_bounded() {
    with_isolated_ipc_test(|| {
        let endpoint = super::create_endpoint().expect("create endpoint");
        for index in 0..super::MAX_ENDPOINT_PENDING_MESSAGES {
            let request = [index as u8 + 1];
            super::enqueue_endpoint_call(endpoint, index as u64, &request)
                .expect("enqueue within limit");
        }
        assert_eq!(
            super::enqueue_endpoint_call(endpoint, 1000, b"x"),
            Err(IpcError::NoMemory)
        );
    });
}

#[test]
fn endpoint_recv_capacity_preserves_front_message() {
    with_isolated_ipc_test(|| {
        let endpoint = super::create_endpoint().expect("create endpoint");
        super::enqueue_endpoint_call(endpoint, 3, b"long-request").expect("enqueue call");
        assert_eq!(
            recv_endpoint_with_limits(endpoint, 4),
            Err(IpcError::BufferTooSmall)
        );
        let (_reply, request) = recv_endpoint(endpoint)
            .expect("recv endpoint")
            .expect("message queued");
        assert_eq!(request, b"long-request");
    });
}

#[test]
fn endpoint_rejects_malformed_message_lengths() {
    with_isolated_ipc_test(|| {
        let endpoint = super::create_endpoint().expect("create endpoint");
        assert_eq!(
            super::enqueue_endpoint_call(endpoint, 1, b""),
            Err(IpcError::InvalidArgument)
        );
        let oversized = alloc::vec![0_u8; super::MAX_ENDPOINT_INLINE_MESSAGE_BYTES + 1];
        assert_eq!(
            super::enqueue_endpoint_call(endpoint, 1, oversized.as_slice()),
            Err(IpcError::InvalidArgument)
        );
    });
}

#[test]
fn endpoint_owner_exit_fails_pending_callers() {
    with_isolated_ipc_test(|| {
        let endpoint = super::create_endpoint_for_task(Some(10)).expect("create endpoint");
        let (reply, _) =
            super::enqueue_endpoint_call(endpoint, 22, b"request").expect("enqueue call");

        let wake_set = super::fail_endpoints_owned_by_task(10, IpcError::PeerClosed);
        assert_eq!(wake_set.callers(), &[22]);
        assert!(wake_set.receivers().is_empty());
        assert_eq!(
            super::take_endpoint_response(reply),
            Err(IpcError::PeerClosed)
        );
        assert_eq!(
            super::enqueue_endpoint_call(endpoint, 23, b"request"),
            Err(IpcError::InvalidHandle)
        );
    });
}

#[test]
fn process_owner_exit_fails_pending_callers() {
    with_isolated_ipc_test(|| {
        let endpoint = super::create_endpoint_for_process(10).expect("create endpoint");
        let (reply, _) =
            super::enqueue_endpoint_call(endpoint, 22, b"request").expect("enqueue call");

        let wake_set = super::fail_endpoints_owned_by_process(10, IpcError::PeerClosed);
        assert_eq!(wake_set.callers(), &[22]);
        assert_eq!(
            super::take_endpoint_response(reply),
            Err(IpcError::PeerClosed)
        );
    });
}

#[test]
fn endpoint_peer_close_returns_unreceived_request_handles_for_cleanup() {
    with_isolated_ipc_test(|| {
        let endpoint = super::create_endpoint_for_task(Some(10)).expect("create endpoint");
        let handle = transferable_file_handle(77);
        let (reply, _) =
            super::enqueue_endpoint_call_with_handles(endpoint, 22, b"request", &[handle])
                .expect("enqueue call");

        let wake_set = super::fail_endpoints_owned_by_task(10, IpcError::PeerClosed);
        assert_eq!(wake_set.callers(), &[22]);
        assert_eq!(
            super::take_endpoint_response_detailed(reply, 0),
            Ok(super::EndpointResponseTake::Error {
                error: IpcError::PeerClosed,
                discarded_request_handles: alloc::vec![handle],
            })
        );
    });
}

#[test]
fn endpoint_cancel_pending_call_removes_queued_message() {
    with_isolated_ipc_test(|| {
        let endpoint = super::create_endpoint().expect("create endpoint");
        let (stale_reply, _) =
            super::enqueue_endpoint_call(endpoint, 22, b"stale").expect("enqueue stale call");
        let (live_reply, _) =
            super::enqueue_endpoint_call(endpoint, 23, b"live").expect("enqueue live call");

        // Model a caller that has reclaimed its message object between
        // queue selection and receive. The live request behind it must
        // still be delivered after the stale head is consumed exactly once.
        super::set_endpoint_recv_stale_head_fault(true);
        let (server_reply, request) = recv_endpoint(endpoint)
            .expect("receive after stale head")
            .expect("live request behind stale head");
        super::set_endpoint_recv_stale_head_fault(false);
        assert_eq!(server_reply, live_reply);
        assert_eq!(request, b"live");
        assert_eq!(
            super::take_endpoint_response(stale_reply),
            Err(IpcError::InvalidHandle)
        );
        assert_eq!(
            super::complete_endpoint_reply(stale_reply, b"late"),
            Err(IpcError::InvalidHandle)
        );
        assert_eq!(super::complete_endpoint_reply(live_reply, b"ok"), Ok(23));
    });
}

#[test]
fn retiring_caller_returns_all_outstanding_transfer_batches() {
    with_isolated_ipc_test(|| {
        let endpoint = super::create_endpoint().expect("create endpoint");
        let first = transferable_file_handle(81);
        let second = transferable_file_handle(82);
        let (first_reply, _) =
            super::enqueue_endpoint_call_with_handles(endpoint, 22, b"first", &[first])
                .expect("enqueue first");
        let (second_reply, _) =
            super::enqueue_endpoint_call_with_handles(endpoint, 22, b"second", &[second])
                .expect("enqueue second");

        let mut discarded = Vec::new();
        assert_eq!(
            super::cancel_endpoint_calls_for_task(22, |batch| {
                discarded.extend_from_slice(batch);
            }),
            2
        );
        assert_eq!(discarded, alloc::vec![first, second]);
        assert_eq!(recv_endpoint(endpoint), Ok(None));
        assert_eq!(
            super::take_endpoint_response(first_reply),
            Err(IpcError::InvalidHandle)
        );
        assert_eq!(
            super::take_endpoint_response(second_reply),
            Err(IpcError::InvalidHandle)
        );
    });
}

#[test]
fn retiring_caller_may_consume_the_exact_global_message_capacity() {
    with_isolated_ipc_test(|| {
        let first = super::create_endpoint().expect("create first endpoint");
        let second = super::create_endpoint().expect("create second endpoint");
        for endpoint in [first, second] {
            for sequence in 0..super::MAX_ENDPOINT_PENDING_MESSAGES {
                super::enqueue_endpoint_call(endpoint, 22, &[(sequence + 1) as u8])
                    .expect("enqueue within endpoint and global capacity");
            }
        }

        assert_eq!(
            super::cancel_endpoint_calls_for_task(22, |batch| {
                assert!(batch.is_empty());
            }),
            super::MAX_ENDPOINT_MESSAGE_OBJECTS
        );
        assert_eq!(recv_endpoint(first), Ok(None));
        assert_eq!(recv_endpoint(second), Ok(None));
    });
}

#[test]
fn endpoint_cancel_dequeued_call_invalidates_late_reply() {
    with_isolated_ipc_test(|| {
        let endpoint = super::create_endpoint().expect("create endpoint");
        let (reply, _) =
            super::enqueue_endpoint_call(endpoint, 22, b"request").expect("enqueue call");
        let (server_reply, request) = recv_endpoint(endpoint)
            .expect("recv endpoint")
            .expect("message queued");
        assert_eq!(server_reply, reply);
        assert_eq!(request, b"request");

        assert_eq!(super::cancel_endpoint_call(reply, 22), Ok(()));
        assert_eq!(
            super::complete_endpoint_reply(reply, b"late"),
            Err(IpcError::InvalidHandle)
        );
        assert_eq!(
            super::take_endpoint_response(reply),
            Err(IpcError::InvalidHandle)
        );
    });
}

#[test]
fn endpoint_cancel_rejects_wrong_caller_without_consuming_reply() {
    with_isolated_ipc_test(|| {
        let endpoint = super::create_endpoint().expect("create endpoint");
        let handle = transferable_file_handle(23);
        let (reply, receiver) =
            super::enqueue_endpoint_call_with_handles(endpoint, 22, b"request", &[handle])
                .expect("enqueue call");
        assert_eq!(receiver, None);

        assert_eq!(
            super::cancel_endpoint_call(reply, 23),
            Err(IpcError::InvalidArgument)
        );

        // A corrupt reply identity is rejected before queue mutation. The
        // reset guard keeps the injected observation test-local.
        super::set_endpoint_cancel_reply_binding_fault(true);
        assert_eq!(
            super::cancel_endpoint_call(reply, 22),
            Err(IpcError::InvalidHandle)
        );
        super::set_endpoint_cancel_reply_binding_fault(false);

        // The failed validation must leave the queued request intact. A
        // later exact retry therefore reclaims it as Queued, not as an
        // already delivered or orphaned in-flight request.
        let cancelled = super::cancel_endpoint_call_with_transfers(reply, 22)
            .expect("caller reclaims the unreceived handle batch");
        assert_eq!(
            cancelled.disposition,
            super::CancelledCallDisposition::Queued
        );
        assert_eq!(cancelled.transfers, alloc::vec![handle]);
        assert_eq!(
            super::take_endpoint_response(reply),
            Err(IpcError::InvalidHandle)
        );
    });
}

#[test]
fn endpoint_owner_exit_wakes_receivers() {
    with_isolated_ipc_test(|| {
        let endpoint = super::create_endpoint_for_task(Some(10)).expect("create endpoint");
        super::add_endpoint_receiver_waiter(endpoint, 31).expect("add waiter");
        super::add_endpoint_receiver_waiter(endpoint, 32).expect("add waiter");

        let wake_set = super::fail_endpoints_owned_by_task(10, IpcError::PeerClosed);
        assert!(wake_set.callers().is_empty());
        assert_eq!(wake_set.receivers(), &[31, 32]);
    });
}

#[test]
fn endpoint_owner_exit_fails_dequeued_call() {
    with_isolated_ipc_test(|| {
        let endpoint = super::create_endpoint_for_task(Some(10)).expect("create endpoint");
        let (reply, _) =
            super::enqueue_endpoint_call(endpoint, 22, b"request").expect("enqueue call");
        let (server_reply, _request) = recv_endpoint(endpoint)
            .expect("recv endpoint")
            .expect("message queued");
        assert_eq!(server_reply, reply);

        let wake_set = super::fail_endpoints_owned_by_task(10, IpcError::PeerClosed);
        assert_eq!(wake_set.callers(), &[22]);
        assert!(wake_set.receivers().is_empty());
        assert_eq!(
            super::complete_endpoint_reply(reply, b"late"),
            Err(IpcError::InvalidArgument)
        );
        assert_eq!(
            super::take_endpoint_response(reply),
            Err(IpcError::PeerClosed)
        );
    });
}

#[test]
fn endpoint_remove_waiters_for_task_prunes_stale_waiters() {
    with_isolated_ipc_test(|| {
        let first = super::create_endpoint().expect("create first endpoint");
        let second = super::create_endpoint().expect("create second endpoint");
        super::add_endpoint_receiver_waiter(first, 9).expect("add first stale waiter");
        super::add_endpoint_receiver_waiter(first, 10).expect("add live waiter");
        super::add_endpoint_receiver_waiter(second, 9).expect("add second stale waiter");

        assert_eq!(super::remove_endpoint_waiters_for_task(9), 2);
        let (_reply, receiver_to_wake) =
            super::enqueue_endpoint_call(first, 22, b"request").expect("enqueue first");
        assert_eq!(receiver_to_wake, Some(10));
        let (_reply, receiver_to_wake) =
            super::enqueue_endpoint_call(second, 23, b"request").expect("enqueue second");
        assert_eq!(receiver_to_wake, None);
    });
}
