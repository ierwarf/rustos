// SPDX-License-Identifier: MIT
use super::super::{
    google_ping_reply_observed, network_exercise_ready, parse_smoke_options,
    rejoin_user_debug_records, render_private_acceptance_contract, DvmNetworkCounters,
};

const DNS: &str = "netprobe: google DNS resolved host=google.com ip=142.250.72.14";
const REPLY: &str = "netprobe: google echo reply host=google.com ip=142.250.72.14 id=1234 seq=1 bytes=32 rtt_us=12345 verified=1";

#[test]
fn google_ping_option_requires_network_and_rejects_physical_net_none() {
    assert!(
        parse_smoke_options(["--exercise-google-ping"].map(str::to_owned).into_iter()).is_err()
    );
    let flags = [
        "--gui-dvm-surfaces",
        "--dvm-network-shmem",
        "--exercise-network",
    ];
    let offline = parse_smoke_options(flags.map(str::to_owned).into_iter()).unwrap();
    assert!(!offline.exercise_google_ping);
    let options = parse_smoke_options(
        flags
            .into_iter()
            .chain(["--exercise-google-ping"])
            .map(str::to_owned),
    )
    .unwrap();
    assert!(options.exercise_network && options.exercise_google_ping);
    let error = parse_smoke_options(
        flags
            .into_iter()
            .chain(["--physical-gpu", "0000:01:00.0"])
            .map(str::to_owned),
    )
    .unwrap_err();
    assert!(error.to_string().contains("-net none"));
}

#[test]
fn google_ping_private_contract_records_explicit_opt_in() {
    assert_eq!(
        render_private_acceptance_contract(false, true, true),
        "contract=rustos-kvm-acceptance-v1\nui_profile=0\nnetwork_exercise=1\ngoogle_ping=1\n"
    );
    assert!(render_private_acceptance_contract(false, true, false).ends_with("google_ping=0\n"));
}

#[test]
fn google_ping_requires_dns_then_matching_verified_echo() {
    let proof = format!("{DNS}\n{REPLY}\n");
    assert!(google_ping_reply_observed(&proof));
    let debugcon = format!("user-debug payload={DNS}\nuser-debug payload=\\n\nuser-debug payload={DNS}\\r\\n\nuser-debug payload={REPLY}\nuser-debug payload=\\n\nuser-debug payload={REPLY}\\r\\n\n");
    assert!(
        google_ping_reply_observed(&debugcon),
        "escaped stdout duplicates must not erase a valid DNS event"
    );
    assert!(
        google_ping_reply_observed(&rejoin_user_debug_records(&debugcon)),
        "the production rejoin stage retains escaped record terminators"
    );
    for invalid in [
        String::new(),
        DNS.to_owned(),
        REPLY.to_owned(),
        format!("{REPLY}\n{DNS}"),
        format!("{DNS}\nnetprobe: qemu gateway reachable\nnetprobe: connect ok"),
        format!("{DNS}\n{}", REPLY.replace("142.250.72.14", "142.250.72.15")),
        proof.replace("host=google.com", "host=example.com"),
        proof.replace("verified=1", "verified=0"),
        proof.replace("id=1234", "id=0"),
        proof.replace("id=1234", "id=65536"),
        proof.replace("seq=1", "seq=2"),
        proof.replace("bytes=32", "bytes=0"),
        proof.replace("rtt_us=12345", "rtt_us=-1"),
        proof.replace("rtt_us=12345", "rtt_us=15000000"),
        proof.replace("rtt_us=12345", "rtt_us=garbage"),
        proof.replace("verified=1", "verified=1 verified=0"),
    ] {
        assert!(!google_ping_reply_observed(&invalid), "accepted: {invalid}");
    }
}

#[test]
fn google_ping_cannot_replace_gateway_or_consumed_rings() {
    let counters = DvmNetworkCounters {
        tx_producer: 2,
        tx_consumer: 2,
        rx_producer: 2,
        rx_consumer: 2,
        dvm_ready: true,
    };
    let gateway = "netprobe: qemu gateway reachable";
    let google = format!("{DNS}\n{REPLY}");
    let all = format!("{gateway}\n{google}");
    assert!(network_exercise_ready(false, false, None, ""));
    assert!(network_exercise_ready(true, false, Some(counters), gateway));
    assert!(!network_exercise_ready(true, true, Some(counters), gateway));
    assert!(!network_exercise_ready(true, true, Some(counters), &google));
    assert!(!network_exercise_ready(true, true, None, &all));
    assert!(!network_exercise_ready(
        true,
        true,
        Some(DvmNetworkCounters {
            rx_consumer: 0,
            ..counters
        }),
        &all
    ));
    assert!(!network_exercise_ready(
        true,
        true,
        Some(DvmNetworkCounters {
            tx_consumer: 0,
            ..counters
        }),
        &all
    ));
    assert!(network_exercise_ready(true, true, Some(counters), &all));
}
