use super::{
    packet_provider_state_from_wire, poll_turn_changes_readiness, settle_inet_socket_publication,
    start_inet_socket_provider_at, NetdIpcRequest, PacketProviderState, PollIngressSingleResult,
    PollResult, INET_READINESS_POLL_INTERVAL, NET_BROKER_PACKET_STATUS_ACTIVE,
    NET_BROKER_PACKET_STATUS_AWAITING_AUTHENTICATED_CONTROL, NET_BROKER_PACKET_STATUS_UNAVAILABLE,
};

#[test]
fn inet_readiness_poll_is_bounded_without_one_millisecond_churn() {
    assert_eq!(INET_READINESS_POLL_INTERVAL.as_millis(), 10);
}

#[test]
fn rejected_prepared_inet_publication_discards_the_unpublished_token_once() {
    let attempts = std::cell::Cell::new(0_u8);
    let discards = std::cell::Cell::new(0_u8);
    assert_eq!(
        settle_inet_socket_publication(
            71,
            || {
                attempts.set(attempts.get() + 1);
                Err(libc::EPIPE)
            },
            |token| {
                assert_eq!(token, 71);
                discards.set(discards.get() + 1);
            },
        ),
        Err(libc::EPIPE)
    );
    assert_eq!(attempts.get(), 1);
    assert_eq!(discards.get(), 1);

    assert_eq!(
        settle_inet_socket_publication(72, || Ok(()), |_| { discards.set(discards.get() + 1) }),
        Ok(())
    );
    assert_eq!(discards.get(), 1, "a bound reply owns subsequent cleanup");
}

#[test]
fn inet_provider_start_deadline_and_token_gate_precede_the_start_closure() {
    // MUTATION-ANCHOR: the closure models `InetStack::add_tcp_socket`; a
    // request must pass both under-lock admission gates before it can
    // mutate provider socket state.
    let starts = std::cell::Cell::new(0_u8);
    let expired = NetdIpcRequest {
        deadline_ns: 100,
        ..NetdIpcRequest::default()
    };
    assert_eq!(
        start_inet_socket_provider_at(&expired, 100, true, || {
            starts.set(starts.get() + 1);
            Ok(1_u8)
        }),
        Err(libc::ETIMEDOUT)
    );
    assert_eq!(starts.get(), 0);

    let live = NetdIpcRequest {
        deadline_ns: 101,
        ..NetdIpcRequest::default()
    };
    assert_eq!(
        start_inet_socket_provider_at(&live, 100, false, || {
            starts.set(starts.get() + 1);
            Ok(2_u8)
        }),
        Err(libc::EAGAIN)
    );
    assert_eq!(starts.get(), 0);

    assert_eq!(
        start_inet_socket_provider_at(&live, 100, true, || {
            starts.set(starts.get() + 1);
            Ok(3_u8)
        }),
        Ok(3)
    );
    assert_eq!(starts.get(), 1);
}

#[test]
fn inet_ingress_publishes_only_socket_state_transitions() {
    assert!(poll_turn_changes_readiness(
        PollIngressSingleResult::SocketStateChanged,
        PollResult::None,
    ));
    assert!(poll_turn_changes_readiness(
        PollIngressSingleResult::PacketProcessed,
        PollResult::SocketStateChanged,
    ));
    assert!(!poll_turn_changes_readiness(
        PollIngressSingleResult::PacketProcessed,
        PollResult::None,
    ));
}

#[test]
fn packet_provider_wire_states_are_explicit_and_fail_closed() {
    assert_eq!(
        packet_provider_state_from_wire(NET_BROKER_PACKET_STATUS_UNAVAILABLE),
        Ok(PacketProviderState::Unavailable)
    );
    assert_eq!(
        packet_provider_state_from_wire(NET_BROKER_PACKET_STATUS_AWAITING_AUTHENTICATED_CONTROL),
        Ok(PacketProviderState::AwaitingAuthenticatedControl)
    );
    assert_eq!(
        packet_provider_state_from_wire(NET_BROKER_PACKET_STATUS_ACTIVE),
        Ok(PacketProviderState::Active)
    );
    assert_eq!(packet_provider_state_from_wire(99), Err(libc::EPROTO));
}
