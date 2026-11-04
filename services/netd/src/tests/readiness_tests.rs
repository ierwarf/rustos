use super::*;
use crate::local_socket_poll_tests::connected_socket;

fn publications() -> Vec<(u64, u64)> {
    TEST_PUBLICATIONS.with(|entries| std::mem::take(&mut *entries.borrow_mut()))
}

fn pair() -> Mutex<NetState> {
    let state = Mutex::new(NetState::new());
    {
        let mut guard = NetStateGuard::new(state.lock().unwrap());
        guard.sockets.insert(1, connected_socket(2));
        guard.sockets.insert(2, connected_socket(1));
    }
    publications();
    state
}

#[test]
fn mutation_and_generation_share_the_lock_even_on_an_error_return() {
    let state = pair();
    let before = state.lock().unwrap().socket_readiness_generation(1);
    let error = (|| {
        let mut guard = NetStateGuard::new(state.lock().unwrap());
        let UnixSocketState::Connected(socket) = &mut guard.sockets.get_mut(&1).unwrap().state
        else {
            unreachable!()
        };
        socket.incoming_bytes = SOCKET_BUFFER_CAPACITY;
        // A response that reads readiness inside the transaction must already
        // carry its new generation, before guard drop / broker publication.
        let generation = guard.socket_readiness_generation(1);
        assert!(generation > before);
        assert!(publications().is_empty());
        libc::EAGAIN
    })();
    assert_eq!(error, libc::EAGAIN);
    let published = publications();
    assert_eq!(
        published.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        vec![1, 2]
    );
    let mut guard = state
        .try_lock()
        .expect("publication must leave no state lock held");
    assert_eq!(guard.socket_readiness_generation(1), published[0].1);
    assert_eq!(guard.socket_readiness_generation(2), published[1].1);
}

#[test]
fn no_op_and_duplicate_close_do_not_publish_but_final_close_does() {
    let state = pair();
    {
        let mut guard = NetStateGuard::new(state.lock().unwrap());
        guard.sockets.get_mut(&1).unwrap().refs += 1;
    }
    assert!(publications().is_empty());
    {
        let mut guard = NetStateGuard::new(state.lock().unwrap());
        guard.sockets.get_mut(&1).unwrap().refs -= 1;
    }
    assert!(publications().is_empty());
    {
        let mut guard = NetStateGuard::new(state.lock().unwrap());
        guard.sockets.remove(&1);
    }
    let published = publications();
    assert_eq!(
        published.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        vec![1, 2]
    );
    let guard = state.lock().unwrap();
    assert!(!guard.readiness_observations.contains_key(&1));
    assert!(!guard.readiness_generations.contains_key(&1));
}

#[test]
fn a_concurrent_poll_cannot_observe_a_mutation_with_the_old_generation() {
    let state = std::sync::Arc::new(pair());
    let before = state.lock().unwrap().socket_readiness_generation(1);
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (finish_tx, finish_rx) = std::sync::mpsc::channel();
    let writer_state = state.clone();
    let writer = thread::spawn(move || {
        let mut guard = NetStateGuard::new(writer_state.lock().unwrap());
        let UnixSocketState::Connected(socket) = &mut guard.sockets.get_mut(&1).unwrap().state
        else {
            unreachable!()
        };
        socket.incoming_bytes = 1;
        ready_tx.send(()).unwrap();
        finish_rx.recv().unwrap();
        drop(guard);
        publications()
    });
    ready_rx.recv().unwrap();
    assert!(state.try_lock().is_err());
    finish_tx.send(()).unwrap();
    let published = writer.join().unwrap();
    let mut guard = NetStateGuard::new(state.lock().unwrap());
    assert_ne!(
        unix_socket_revents(&guard, 1, linux_abi::POLLIN as u32).unwrap(),
        0
    );
    let generation = guard.socket_readiness_generation(1);
    assert!(generation > before);
    assert_eq!(published, vec![(1, generation)]);
}

fn udp_handle(state: &NetState, token: u64) -> SocketHandle {
    state.inet_sockets[&token].transport.handle()
}

#[test]
fn a_named_inet_request_publishes_only_the_socket_whose_readiness_changed() {
    let mut state = bench_state(8, 1);
    let before = state.readiness_generations.clone();
    let socket = state.inet_sockets.get_mut(&1003).unwrap();
    socket.received = Some(inet_datagram::ReceivedPacket::test_packet());
    state.record_readiness_changes();
    let generation = state.socket_readiness_generation(1003);
    assert_eq!(state.readiness_publications, vec![(1003, generation)]);
    assert!(generation > before.get(&1003).copied().unwrap_or(1));
    // A request that changes nothing publishes nothing.
    state.readiness_publications.clear();
    state.inet_sockets.get_mut(&1005).unwrap();
    state.record_readiness_changes();
    assert!(state.readiness_publications.is_empty());
}

#[test]
fn a_stack_advance_stamps_every_inet_socket_it_changed_and_only_those() {
    let mut state = bench_state(8, 0);
    for token in [1002, 1006] {
        let handle = udp_handle(&state, token);
        // Filling the smoltcp transmit queue drops writability without any
        // table-level mutation: only the stack advance can reveal it.
        let destination = IpEndpoint::new(IpAddress::Ipv4(QEMU_USERNET_GATEWAY), 53);
        let socket = state
            .inet
            .as_mut()
            .unwrap()
            .sockets
            .get_mut::<udp::Socket>(handle);
        while socket.send_slice(&[0], destination).is_ok() {}
        assert!(!socket.can_send());
    }
    state.record_readiness_changes();
    assert!(state.readiness_publications.is_empty());
    state.inet.as_mut().unwrap().readiness_dirty = true;
    state.record_readiness_changes();
    let generation = state.readiness_generations[&1002];
    assert_eq!(
        state.readiness_publications,
        vec![(1002, generation), (1006, generation)]
    );
}

#[test]
fn retiring_an_inet_socket_during_a_stack_advance_publishes_its_final_close() {
    let mut state = bench_state(4, 0);
    let transport = state.inet_sockets.remove(&1001).unwrap().transport;
    state.inet.as_mut().unwrap().remove(transport);
    state.inet.as_mut().unwrap().readiness_dirty = true;
    state.record_readiness_changes();
    assert_eq!(state.readiness_publications.len(), 1);
    assert_eq!(state.readiness_publications[0].0, 1001);
    assert!(!state.readiness_observations.contains_key(&1001));
    assert!(!state.readiness_generations.contains_key(&1001));
}

#[test]
fn unix_and_inet_changes_in_one_transaction_publish_in_token_order() {
    let mut state = bench_state(2, 1);
    state.inet_sockets.get_mut(&1000).unwrap().received =
        Some(inet_datagram::ReceivedPacket::test_packet());
    let UnixSocketState::Connected(socket) = &mut state.sockets.get_mut(&10).unwrap().state else {
        unreachable!()
    };
    socket.incoming_bytes = SOCKET_BUFFER_CAPACITY;
    state.record_readiness_changes();
    let tokens: Vec<u64> = state.readiness_publications.iter().map(|p| p.0).collect();
    assert_eq!(tokens, vec![10, 11, 1000]);
    assert!(state.readiness_publications.windows(2).all(|w| w[0].1 == w[1].1));
}

fn bench_state(inet: usize, unix_pairs: u64) -> NetState {
    let mut device = BrokerDevice {
        rx: [0; NET_BROKER_PACKET_MTU],
        errors: inet_datagram::ErrorIngress::default(),
    };
    let config = SmolConfig::new(HardwareAddress::Ethernet(QEMU_USERNET_MAC));
    let mut iface = Interface::new(config, &mut device, SmolInstant::from_millis(0));
    iface.update_ip_addrs(|a| {
        a.push(IpCidr::new(IpAddress::Ipv4(QEMU_USERNET_ADDR), 24))
            .unwrap();
    });
    let mut stack = InetStack {
        iface,
        device,
        sockets: SocketSet::new(Vec::new()),
        next_port: 49152,
        closing: Vec::new(),
        readiness_dirty: false,
        last_timer_sweep: StdInstant::now(),
    };
    let mut state = NetState::new();
    for i in 0..inet as u64 {
        let transport = inet_datagram::add_socket(&mut stack, linux_abi::SOCK_DGRAM, 17);
        if let InetTransport::Udp(handle) = transport {
            stack
                .sockets
                .get_mut::<udp::Socket>(handle)
                .bind(smoltcp::wire::IpListenEndpoint {
                    addr: None,
                    port: 40000 + i as u16,
                })
                .unwrap();
        }
        state.inet_sockets.insert(
            1000 + i,
            InetSocket {
                refs: 1,
                options: SocketOptions::default(),
                transport,
                local: None,
                peer: None,
                received: None,
                errors: None,
                connect_deadline: None,
            },
        );
    }
    for p in 0..unix_pairs {
        state.sockets.insert(10 + 2 * p, connected_socket(11 + 2 * p));
        state.sockets.insert(11 + 2 * p, connected_socket(10 + 2 * p));
    }
    state.inet = Some(stack);
    state.record_readiness_changes();
    state.readiness_publications.clear();
    state
}

/// Manual: `cargo test -p netd --release readiness_bench -- --ignored --nocapture`.
#[test]
#[ignore]
fn readiness_bench() {
    for inet in [1usize, 16, 256] {
        let mut state = bench_state(inet, 4);
        let token = 1000 + (inet as u64) / 2;
        let iterations = 200_000 / inet.max(1) as u32 + 2000;
        let mut time = |label: &str, mut body: Box<dyn FnMut(&mut NetState) + '_>| {
            for _ in 0..iterations / 10 {
                body(&mut state);
            }
            let start = std::time::Instant::now();
            for _ in 0..iterations {
                body(&mut state);
            }
            let ns = start.elapsed().as_nanos() as f64 / f64::from(iterations);
            println!("readiness_bench inet={inet:<4} {label:<22} {ns:>10.0} ns/call");
            assert!(state.readiness_publications.is_empty());
        };
        time(
            "single-socket request",
            Box::new(move |state| {
                state.inet_sockets.get_mut(&token).unwrap();
                state.record_readiness_changes();
            }),
        );
        time(
            "stack advance",
            Box::new(|state| {
                state.inet.as_mut().unwrap().readiness_dirty = true;
                state.record_readiness_changes();
            }),
        );
        time(
            "unix-only request",
            Box::new(|state| {
                state.sockets.get_mut(&10).unwrap();
                state.record_readiness_changes();
            }),
        );
    }
}

#[test]
fn quiet_poll_turns_do_not_force_a_full_inet_sweep_until_the_timer_is_due() {
    let mut state = bench_state(4, 0);
    state.record_readiness_changes();
    let stack = state.inet.as_mut().unwrap();
    stack.last_timer_sweep = StdInstant::now();
    stack.readiness_dirty = false;
    // Host tests have no packet broker: ingress and egress are both idle.
    let (_, changed) = stack.poll_one();
    assert!(!changed);
    assert!(!stack.readiness_dirty, "an idle poll must not request a sweep");

    // A silent timer-driven transition is still found within the bound.
    let Some(overdue) = StdInstant::now().checked_sub(INET_TIMER_SWEEP_INTERVAL * 2) else {
        return;
    };
    stack.last_timer_sweep = overdue;
    stack.poll_one();
    assert!(stack.readiness_dirty);
    assert!(stack.last_timer_sweep > overdue);
    stack.readiness_dirty = false;
    stack.poll_one();
    assert!(!stack.readiness_dirty, "the sweep timer restarts after it fires");
}
