use super::*;

fn arbitration_fixture(
    source_actions: u32,
) -> (
    wayland_server::Display<super::super::WaylandState>,
    super::super::WaylandState,
    std::os::unix::net::UnixStream,
    std::os::unix::net::UnixStream,
    Vec<(u32, u16, Vec<u8>)>,
) {
    let mut display = test_display();
    super::super::create_wayland_globals(&display.handle());
    let mut state = super::super::WaylandState::new(1024, 768);
    state.display_handle = Some(display.handle());
    let (server_a, mut a) = std::os::unix::net::UnixStream::pair().unwrap();
    let (server_b, mut b) = std::os::unix::net::UnixStream::pair().unwrap();
    for server in [server_a, server_b] {
        display
            .handle()
            .insert_client(
                server,
                Arc::new(super::super::WaylandClientState::default()),
            )
            .unwrap();
    }
    for peer in [&mut a, &mut b] {
        wire::send(peer, &wire::message(1, 1, &wire::u32s(&[2])));
    }
    pump(&mut display, &mut state);
    for (peer, manager_version) in [(&mut a, 3), (&mut b, 2)] {
        let mut names = std::collections::HashMap::new();
        for (obj, op, p) in wire::events(peer) {
            if obj == 2 && op == 0 {
                let name = u32::from_ne_bytes(p[0..4].try_into().unwrap());
                let len = u32::from_ne_bytes(p[4..8].try_into().unwrap()) as usize;
                names.insert(String::from_utf8(p[8..8 + len - 1].to_vec()).unwrap(), name);
            }
        }
        for (iface, ver, id) in [
            ("wl_compositor", 6, 3),
            ("wl_seat", 7, 4),
            ("wl_data_device_manager", manager_version, 5),
        ] {
            let mut args = wire::u32s(&[names[iface]]);
            args.extend(wire::string(iface));
            args.extend(wire::u32s(&[ver, id]));
            wire::send(peer, &wire::message(2, 0, &args));
        }
        wire::send(peer, &wire::message(3, 0, &wire::u32s(&[6])));
        wire::send(peer, &wire::message(5, 1, &wire::u32s(&[7, 4])));
        if manager_version == 2 {
            let mut args = wire::u32s(&[names["wl_data_device_manager"]]);
            args.extend(wire::string("wl_data_device_manager"));
            args.extend(wire::u32s(&[3, 8]));
            wire::send(peer, &wire::message(2, 0, &args));
            wire::send(peer, &wire::message(8, 1, &wire::u32s(&[9, 4])));
            wire::send(peer, &wire::message(5, 1, &wire::u32s(&[10, 4])));
            wire::send(peer, &wire::message(8, 1, &wire::u32s(&[11, 4])));
        }
    }
    pump(&mut display, &mut state);
    wire::events(&mut a);
    wire::events(&mut b);
    for (index, surface) in state.surfaces.iter().enumerate() {
        let mut surface = surface.shared.lock().unwrap();
        surface.frame = Rect {
            x: 10 + index * 100,
            y: 10,
            width: 50,
            height: 50,
        };
        surface.decoration_server = false;
        surface.width = 50;
        surface.height = 50;
        surface.stride_pixels = 50;
        surface.pixels = Arc::new(vec![0xff00_0000; 2500]);
    }
    let first = state.surfaces[0].shared.lock().unwrap().surface_id;
    let second = state.surfaces[1].shared.lock().unwrap().surface_id;
    state.pointer_target = Some(first);
    state.pointer_x = 20;
    state.pointer_y = 20;
    assert!(state.pointer_button(1, true));
    let serial = state.pointer_grab_serial.unwrap();
    wire::send(&mut a, &wire::message(5, 0, &wire::u32s(&[8])));
    wire::send(&mut a, &wire::message(8, 0, &wire::string("text/plain")));
    wire::send(&mut a, &wire::message(8, 2, &wire::u32s(&[source_actions])));
    wire::send(
        &mut a,
        &wire::message(7, 0, &wire::u32s(&[8, 6, 0, serial])),
    );
    pump(&mut display, &mut state);
    state.pointer_target = Some(second);
    state.pointer_motion(120, 20);
    display.flush_clients().unwrap();
    let events = wire::events(&mut b);
    (display, state, a, b, events)
}

fn drag_offer_and_serial(events: &[(u32, u16, Vec<u8>)], device: u32) -> (u32, u32) {
    let find = |opcode| {
        events
            .iter()
            .find(|(obj, op, _)| *obj == device && *op == opcode)
            .map(|(_, _, payload)| u32::from_ne_bytes(payload[..4].try_into().unwrap()))
            .unwrap()
    };
    (find(0), find(1))
}

fn accept_drag_offer(
    peer: &mut std::os::unix::net::UnixStream,
    offer: u32,
    serial: u32,
    actions: u32,
    preferred: u32,
) {
    let mut accept = wire::u32s(&[serial]);
    accept.extend(wire::string("text/plain"));
    wire::send(peer, &wire::message(offer, 0, &accept));
    wire::send(
        peer,
        &wire::message(offer, 4, &wire::u32s(&[actions, preferred])),
    );
}

#[test]
fn drag_winner_ignores_incompatible_negotiation_in_both_orders() {
    use std::os::fd::AsRawFd;
    for modern_wins in [false, true] {
        for incompatible_first in [false, true] {
            let (mut display, mut state, mut source, mut target, events) = arbitration_fixture(1);
            let (legacy, _) = drag_offer_and_serial(&events, 7);
            let (modern, modern_serial) = drag_offer_and_serial(&events, 9);
            let (incompatible, incompatible_serial) = drag_offer_and_serial(&events, 11);
            if incompatible_first {
                accept_drag_offer(&mut target, incompatible, incompatible_serial, 2, 2);
            }
            if modern_wins {
                accept_drag_offer(&mut target, modern, modern_serial, 1, 1);
            }
            if !incompatible_first {
                accept_drag_offer(&mut target, incompatible, incompatible_serial, 2, 2);
            }
            pump(&mut display, &mut state);
            let source_events = wire::events(&mut source);
            assert!(
                !source_events
                    .iter()
                    .any(|(obj, op, p)| *obj == 8 && *op == 5 && p == &wire::u32s(&[0])),
                "incompatible offer must not clear viable source action"
            );
            assert!(state.pointer_button(1, false));
            display.flush_clients().unwrap();
            let drops = wire::events(&mut target);
            let winner = if modern_wins { 9 } else { 7 };
            for device in [7, 9, 10, 11] {
                assert_eq!(
                    drops.iter().any(|(obj, op, _)| *obj == device && *op == 4),
                    device == winner
                );
                assert!(drops.iter().any(|(obj, op, _)| *obj == device && *op == 2));
            }
            let source_events = wire::events(&mut source);
            assert!(!source_events
                .iter()
                .any(|(obj, op, _)| *obj == 8 && (*op == 2 || *op == 4)));
            if modern_wins {
                wire::send(&mut target, &wire::message(modern, 3, &[]));
            } else {
                // Legacy v2 may omit accept, and delayed receive precedes completion.
                let (_read, write) = std::os::unix::net::UnixStream::pair().unwrap();
                wire::send_fd(
                    &target,
                    &wire::message(legacy, 1, &wire::string("text/plain")),
                    write.as_raw_fd(),
                );
                pump(&mut display, &mut state);
                let events = wire::events(&mut source);
                assert!(events.iter().any(|(obj, op, _)| *obj == 8 && *op == 1));
                assert!(!events.iter().any(|(obj, op, _)| *obj == 8 && *op == 4));
                wire::send(&mut target, &wire::message(legacy, 2, &[]));
            }
            pump(&mut display, &mut state);
            let events = wire::events(&mut source);
            assert_eq!(
                events
                    .iter()
                    .filter(|(obj, op, _)| *obj == 8 && *op == 4)
                    .count(),
                1
            );
            assert!(!events.iter().any(|(obj, op, _)| *obj == 8 && *op == 2));
        }
    }
}

#[test]
fn two_ask_offers_choose_one_winner_for_finish_and_destroy_orders() {
    for negotiation_reversed in [false, true] {
        for loser_destroyed_first in [false, true] {
            for winner_finishes in [false, true] {
                let (mut display, mut state, mut source, mut target, events) =
                    arbitration_fixture(5);
                let winner = drag_offer_and_serial(&events, 9);
                let loser = drag_offer_and_serial(&events, 11);
                let order = if negotiation_reversed {
                    [loser, winner]
                } else {
                    [winner, loser]
                };
                for (offer, serial) in order {
                    accept_drag_offer(&mut target, offer, serial, 4, 4);
                }
                pump(&mut display, &mut state);
                wire::events(&mut source);
                assert!(state.pointer_button(1, false));
                display.flush_clients().unwrap();
                let events = wire::events(&mut target);
                for device in [7, 9, 10, 11] {
                    assert_eq!(
                        events.iter().any(|(obj, op, _)| *obj == device && *op == 4),
                        device == 9
                    );
                    assert!(events.iter().any(|(obj, op, _)| *obj == device && *op == 2));
                }
                let events = wire::events(&mut source);
                assert_eq!(
                    events
                        .iter()
                        .filter(|(obj, op, _)| *obj == 8 && *op == 3)
                        .count(),
                    1
                );
                if loser_destroyed_first {
                    wire::send(&mut target, &wire::message(loser.0, 2, &[]));
                    pump(&mut display, &mut state);
                    assert!(!wire::events(&mut source)
                        .iter()
                        .any(|(obj, op, _)| *obj == 8 && (*op == 2 || *op == 4)));
                }
                if winner_finishes {
                    wire::send(
                        &mut target,
                        &wire::message(winner.0, 4, &wire::u32s(&[1, 1])),
                    );
                    wire::send(&mut target, &wire::message(winner.0, 3, &[]));
                } else {
                    wire::send(&mut target, &wire::message(winner.0, 2, &[]));
                }
                pump(&mut display, &mut state);
                let events = wire::events(&mut source);
                assert_eq!(
                    events
                        .iter()
                        .filter(
                            |(obj, op, _)| *obj == 8 && *op == if winner_finishes { 4 } else { 2 }
                        )
                        .count(),
                    1
                );
                if !loser_destroyed_first {
                    wire::send(&mut target, &wire::message(loser.0, 2, &[]));
                    pump(&mut display, &mut state);
                    assert!(!wire::events(&mut source)
                        .iter()
                        .any(|(obj, op, _)| *obj == 8 && (*op == 2 || *op == 4)));
                }
            }
        }
    }
}

#[test]
fn legacy_drag_winner_completes_on_client_disconnect() {
    let (mut display, mut state, mut source, target, _events) = arbitration_fixture(1);
    wire::events(&mut source);
    assert!(state.pointer_button(1, false));
    display.flush_clients().unwrap();
    assert!(!wire::events(&mut source)
        .iter()
        .any(|(obj, op, _)| *obj == 8 && *op == 4));
    drop(target);
    pump(&mut display, &mut state);
    let events = wire::events(&mut source);
    assert_eq!(
        events
            .iter()
            .filter(|(obj, op, _)| *obj == 8 && *op == 4)
            .count(),
        1
    );
}

#[test]
fn resolved_ask_action_immediately_precedes_finished_after_transfer_requests() {
    use std::os::fd::AsRawFd;
    let (mut display, mut state, mut source, mut target, events) = arbitration_fixture(5);
    let (offer, serial) = drag_offer_and_serial(&events, 9);
    accept_drag_offer(&mut target, offer, serial, 4, 4);
    pump(&mut display, &mut state);
    wire::events(&mut source);
    assert!(state.pointer_button(1, false));
    display.flush_clients().unwrap();
    wire::events(&mut source);
    wire::events(&mut target);

    // Resolving ask must not notify the source until successful finish.
    wire::send(&mut target, &wire::message(offer, 4, &wire::u32s(&[1, 1])));
    pump(&mut display, &mut state);
    assert!(!wire::events(&mut source)
        .iter()
        .any(|(obj, op, _)| *obj == 8 && (*op == 5 || *op == 4)));

    let (_read, write) = std::os::unix::net::UnixStream::pair().unwrap();
    wire::send_fd(
        &target,
        &wire::message(offer, 1, &wire::string("text/plain")),
        write.as_raw_fd(),
    );
    let mut accept = wire::u32s(&[serial]);
    accept.extend(wire::string("text/plain"));
    wire::send(&mut target, &wire::message(offer, 0, &accept));
    wire::send(&mut target, &wire::message(offer, 3, &[]));
    pump(&mut display, &mut state);
    let events: Vec<_> = wire::events(&mut source)
        .into_iter()
        .filter(|(obj, _, _)| *obj == 8)
        .collect();
    assert_eq!(
        events.iter().map(|(_, op, _)| *op).collect::<Vec<_>>(),
        [1, 0, 5, 4]
    );
    assert_eq!(events[2].2, wire::u32s(&[1]));
    wire::send(&mut target, &wire::message(offer, 2, &[]));
    pump(&mut display, &mut state);
    assert!(!wire::events(&mut source)
        .iter()
        .any(|(obj, op, _)| *obj == 8 && (*op == 5 || *op == 4)));
}
