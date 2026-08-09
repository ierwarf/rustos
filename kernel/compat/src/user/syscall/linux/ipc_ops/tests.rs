use super::*;

#[test]
fn public_ipc_calls_share_the_finite_service_deadline() {
    const {
        assert!(SERVICE_IPC_TIMEOUT_MS > 0);
        assert!(SERVICE_IPC_TIMEOUT_MS <= IPC_WAIT_SERVICE_ENDPOINT_MAX_TIMEOUT_MS);
    }
    assert_eq!(
        ServiceIpcClass::ReadinessQuery.timeout_ms(),
        rustos_user_abi::performance::IPC_READINESS_QUERY_HARD_LIMIT_MS
    );
    assert_eq!(
        ServiceIpcClass::InteractiveControl.timeout_ms(),
        rustos_user_abi::performance::IPC_INTERACTIVE_CONTROL_HARD_LIMIT_MS
    );
    assert_eq!(
        ServiceIpcClass::BootControl.timeout_ms(),
        rustos_user_abi::performance::IPC_BOOT_CONTROL_HARD_LIMIT_MS
    );
    assert_eq!(
        ServiceIpcClass::BulkData.timeout_ms(),
        SERVICE_IPC_TIMEOUT_MS
    );
    assert_eq!(
        ServiceIpcClass::ReadinessQuery.cap_timeout_ms(u64::MAX),
        rustos_user_abi::performance::IPC_READINESS_QUERY_HARD_LIMIT_MS
    );
    assert!(!bounded_ipc_call_timeout_is_valid(0));
    assert!(bounded_ipc_call_timeout_is_valid(
        rustos_user_abi::performance::IPC_INTERACTIVE_CONTROL_HARD_LIMIT_MS
    ));
    assert!(!bounded_ipc_call_timeout_is_valid(
        SERVICE_IPC_TIMEOUT_MS + 1
    ));
    assert_eq!(
        rustos_user_abi::syscall::SYS_RUSTOS_IPC_CALL_WITH_HANDLES_BOUNDED,
        0x5255_0045
    );
    assert_eq!(ServiceIpcClass::BootControl.cap_timeout_ms(37), 37);
    assert_eq!(ServiceIpcClass::InteractiveControl.cap_timeout_ms(0), 1);
}

#[test]
fn netd_wire_deadline_maps_to_the_same_monotonic_reply_tick() {
    let request = NetdIpcRequest {
        version: NETD_IPC_ABI_VERSION,
        deadline_ns: 19_999_999_999,
        ..NetdIpcRequest::default()
    };
    let request_bytes = as_bytes(&request);

    assert_eq!(
        netd_deadline_tick_from_request(request_bytes, 1_000),
        Some(19_999)
    );
    assert_eq!(
        netd_deadline_tick_from_request(request_bytes, 2_000),
        Some(39_999)
    );
    // A caller-controlled wire end is an upper bound on the kernel's
    // existing service class, never authority to widen it.
    assert_eq!(bounded_netd_reply_deadline_tick(7, request_bytes, 1_000), 7);
    assert_eq!(
        bounded_netd_reply_deadline_tick(30_000, request_bytes, 1_000),
        19_999
    );

    let zero = NetdIpcRequest::default();
    assert_eq!(
        netd_deadline_tick_from_request(as_bytes(&zero), 1_000),
        None
    );
    assert_eq!(
        bounded_netd_reply_deadline_tick(7, as_bytes(&zero), 1_000),
        7
    );
    let v6 = NetdIpcRequest {
        version: NETD_IPC_ABI_VERSION - 1,
        ..request
    };
    assert_eq!(bounded_netd_reply_deadline_tick(7, as_bytes(&v6), 1_000), 7);
    assert_eq!(
        bounded_netd_reply_deadline_tick(
            7,
            &request_bytes[..core::mem::offset_of!(NetdIpcRequest, deadline_ns)],
            1_000,
        ),
        7
    );
}

#[test]
fn reply_observation_orders_deadline_before_a_late_queue_response() {
    // The two deterministic samples model pre-expired, expiry during the
    // queue take, and a response observed entirely before the deadline.
    assert!(!rustos_user_abi::deadline::reply_observation_allows_publication(true, false));
    assert!(!rustos_user_abi::deadline::reply_observation_allows_publication(true, true));
    assert!(!rustos_user_abi::deadline::reply_observation_allows_publication(false, true));
    assert!(rustos_user_abi::deadline::reply_observation_allows_publication(false, false));
}

#[test]
fn retired_task_cleanup_removes_service_endpoint_waiter_exactly_once() {
    let task_id = u64::MAX - 401;
    assert!(register_service_endpoint_waiter(ServiceEndpointWaiter {
        task_id,
        service_id: linux_abi::IPC_SERVICE_VFSD,
        expected_pid: u64::MAX - 402,
    }));
    assert_eq!(remove_service_endpoint_waiter(task_id), 1);
    assert_eq!(remove_service_endpoint_waiter(task_id), 0);
}

#[test]
fn service_endpoint_waiter_rearm_replaces_without_allocating_another_slot() {
    let mut table = ServiceEndpointWaiterTable::new();
    assert!(table.register(ServiceEndpointWaiter {
        task_id: 41,
        service_id: linux_abi::IPC_SERVICE_VFSD,
        expected_pid: 51,
    }));
    assert!(table.register(ServiceEndpointWaiter {
        task_id: 41,
        service_id: linux_abi::IPC_SERVICE_NETD,
        expected_pid: 61,
    }));
    let (_, old_count) = table.take_matching(|waiter| waiter.expected_pid == 51);
    assert_eq!(old_count, 0);
    let (tasks, new_count) = table.take_matching(|waiter| waiter.expected_pid == 61);
    assert_eq!(new_count, 1);
    assert_eq!(tasks[0], 41);
}

#[test]
#[allow(
    clippy::assertions_on_constants,
    reason = "the mutation witness must compile a reduced capacity and fail at runtime"
)]
fn service_endpoint_waiter_capacity_covers_every_scheduler_task() {
    assert!(MAX_SERVICE_ENDPOINT_WAITERS >= multitask::MAX_SCHEDULER_TASKS);
}

#[test]
fn service_endpoint_epoch_changes_on_every_publication_boundary() {
    assert_eq!(next_service_endpoint_epoch(0), Some(1));
    assert_eq!(next_service_endpoint_epoch(1), Some(2));
    assert_eq!(next_service_endpoint_epoch(u64::MAX), None);
}

#[test]
fn stable_service_endpoint_snapshot_rejects_revoked_owners() {
    assert_eq!(SERVICE_ENDPOINT_STABLE_READ_ATTEMPTS, 3);
    assert_eq!(stable_service_endpoint_snapshot(47, 41, false), 47);
    assert_eq!(stable_service_endpoint_snapshot(0, 41, false), 0);
    assert_eq!(stable_service_endpoint_snapshot(47, 0, false), 0);
    assert_eq!(stable_service_endpoint_snapshot(47, 41, true), 0);
}

#[test]
fn cached_service_call_grant_is_exact_process_and_epoch() {
    assert!(cached_service_call_grant_matches(41, 7, 41, 7));
    assert!(!cached_service_call_grant_matches(41, 7, 42, 7));
    assert!(!cached_service_call_grant_matches(41, 7, 41, 8));
    assert!(!cached_service_call_grant_matches(0, 7, 0, 7));
    assert!(!cached_service_call_grant_matches(41, 0, 41, 0));
}

#[test]
fn inputd_owner_exit_withdraws_the_separate_ring_policy_lease() {
    assert!(service_exit_requires_input_policy_withdrawal(
        linux_abi::IPC_SERVICE_INPUTD
    ));
    assert!(!service_exit_requires_input_policy_withdrawal(
        linux_abi::IPC_SERVICE_NETD
    ));
}

#[test]
fn root_service_publication_is_boot_owner_sealed_and_epoch_bound() {
    assert!(rootd_bootstrap_owner_allows(0, 41));
    assert!(rootd_bootstrap_owner_allows(41, 41));
    assert!(!rootd_bootstrap_owner_allows(41, 42));
    assert!(!rootd_bootstrap_owner_allows(0, 0));

    assert!(rootd_authorization_epoch_matches(7, 101, 41, 7, false));
    assert!(!rootd_authorization_epoch_matches(7, 101, 41, 8, false));
    assert!(!rootd_authorization_epoch_matches(7, 0, 41, 7, false));
    assert!(!rootd_authorization_epoch_matches(7, 101, 41, 7, true));
}

#[test]
fn service_call_grants_are_exact_epoch_bounded_and_revocable() {
    let mut grants = [ServiceCallGrant::empty(); 2];
    assert_eq!(record_service_call_grant(&mut grants, 41, 3, 7), Ok(()));
    assert!(has_service_call_grant(&grants, 41, 3, 7));
    assert!(!has_service_call_grant(&grants, 42, 3, 7));
    assert!(!has_service_call_grant(&grants, 41, 3, 8));

    assert_eq!(record_service_call_grant(&mut grants, 41, 3, 8), Ok(()));
    assert!(!has_service_call_grant(&grants, 41, 3, 7));
    assert!(has_service_call_grant(&grants, 41, 3, 8));

    assert_eq!(record_service_call_grant(&mut grants, 42, 4, 9), Ok(()));
    assert_eq!(
        record_service_call_grant(&mut grants, 43, 5, 10),
        Err(LINUX_ENOSPC)
    );
    clear_service_call_grants(&mut grants, 41);
    assert!(!has_service_call_grant(&grants, 41, 3, 8));
    assert!(has_service_call_grant(&grants, 42, 4, 9));
}

fn matching_commercial_response() -> (CommercialMaxProtocolRequest, CommercialMaxProtocolResponse) {
    let mut request = CommercialMaxProtocolRequest::default();
    request.header.protocol = rustos_user_abi::syscall::COMMERCIAL_MAX_PROTOCOL_ROOTD_SUPERVISOR;
    request.header.op = rustos_user_abi::syscall::COMMERCIAL_MAX_ROOTD_OP_SERVICE_LOOKUP;
    request.header.service_id = linux_abi::IPC_SERVICE_ROOTD;
    request.header.subject_pid = 41;
    request.header.subject_tid = 43;
    request.header.ticket = 47;
    let response = CommercialMaxProtocolResponse {
        header: request.header,
        ..CommercialMaxProtocolResponse::default()
    };
    (request, response)
}

#[test]
fn commercial_response_envelope_is_bound_to_request_and_bounded() {
    let (request, response) = matching_commercial_response();
    assert_eq!(
        validate_commercial_response_envelope(&request, &response),
        Ok(())
    );

    let mut wrong_subject = response;
    wrong_subject.header.subject_tid += 1;
    assert_eq!(
        validate_commercial_response_envelope(&request, &wrong_subject),
        Err(LINUX_EINVAL)
    );

    let mut reserved = response;
    reserved.reserved1 = 1;
    assert_eq!(
        validate_commercial_response_envelope(&request, &reserved),
        Err(LINUX_EINVAL)
    );

    let mut too_many_descriptors = response;
    too_many_descriptors.descriptor_count = (too_many_descriptors.descriptors.len() + 1) as u16;
    assert_eq!(
        validate_commercial_response_envelope(&request, &too_many_descriptors),
        Err(LINUX_EINVAL)
    );

    let mut oversized_capability_label = response;
    oversized_capability_label.capability.label_len =
        (oversized_capability_label.capability.label.len() + 1) as u16;
    assert_eq!(
        validate_commercial_response_envelope(&request, &oversized_capability_label),
        Err(LINUX_EINVAL)
    );

    let mut malformed_descriptor = response;
    malformed_descriptor.descriptor_count = 1;
    malformed_descriptor.descriptors[0].reserved0 = 1;
    assert_eq!(
        validate_commercial_response_envelope(&request, &malformed_descriptor),
        Err(LINUX_EINVAL)
    );
}
