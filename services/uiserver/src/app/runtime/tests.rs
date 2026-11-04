use super::CONSOLE_GRAPH_WAIT;
use rustos_user_abi::performance::IPC_INTERACTIVE_CONTROL_HARD_LIMIT_MS;
use std::time::Duration;

/// The broker holds this wait inside a call kernel compat will abandon at
/// its class deadline. A re-arm interval at or above that deadline would
/// turn every idle wait into a timeout, and the worker would be back to
/// looking on a timer - with an error path for a cadence.
#[test]
fn the_graph_wait_re_arms_inside_the_class_deadline_that_carries_it() {
    assert!(CONSOLE_GRAPH_WAIT < Duration::from_millis(IPC_INTERACTIVE_CONTROL_HARD_LIMIT_MS));
    // And it has to be a real park, or the interval poll it replaced is
    // still being paid under a new name.
    assert!(CONSOLE_GRAPH_WAIT >= super::CONSOLE_POLL_SLEEP);
}

/// An edge-triggered subscription may only be advanced by an observation
/// that actually completed.
///
/// The broker answers a graph wait once per generation and then holds the
/// reply until the generation moves again. So the token this worker hands
/// back is a claim to have seen everything up to it. Advancing it after a
/// collect that failed, or after a refresh the main loop could not accept,
/// makes that claim falsely - and the output it skipped then waits for an
/// unrelated change to move the generation, one full re-arm interval at a
/// time. `snapshot_us` was measured at 803 ms against ioctls that each took
/// about a millisecond, which is what a run of those looks like.
#[test]
fn only_a_collect_that_fetched_and_delivered_may_consume_the_graph_edge() {
    assert!(super::collect_accounted_for_its_edge(None, true));
    assert!(
        !super::collect_accounted_for_its_edge(Some(libc::EAGAIN), true),
        "a session whose output could not be fetched leaves the edge outstanding"
    );
    assert!(
        !super::collect_accounted_for_its_edge(None, false),
        "a refresh the main loop refused has reached nothing and accounts for nothing"
    );
    assert!(!super::collect_accounted_for_its_edge(
        Some(libc::EAGAIN),
        false
    ));
}

/// The retry that a withheld token produces must be paced.
///
/// Not consuming the edge is what makes the next wait return immediately,
/// which is exactly right for a transient failure and would be a full-speed
/// spin against the console broker for a persistent one.
#[test]
fn a_persistently_failing_collect_is_paced_rather_than_spun() {
    assert!(super::MAX_UNPACED_COLLECT_RETRIES >= 1);
    assert!(
        super::CONSOLE_POLL_SLEEP > Duration::ZERO,
        "the paced retry has to actually yield"
    );
}

/// A refusal and a hiccup must not share a branch.
///
/// They did, and it cost three releases: the kernel guard admitted only a
/// handle the compositor cannot hold, so every wait returned `ENOTTY`, and
/// the retry branch turned that permanent refusal into a `CONSOLE_POLL_SLEEP`
/// cadence that looked exactly like an idle console. Keystroke echo was
/// measured at a 21 ms median against a rail that had never once answered.
#[test]
fn a_refused_graph_wait_is_never_mistaken_for_a_broker_hiccup() {
    for transient in [
        libc::ETIMEDOUT,
        libc::EAGAIN,
        libc::EINTR,
        libc::ENODEV,
        libc::ECONNRESET,
    ] {
        assert!(
            super::console_graph_wait_is_retryable(transient),
            "errno {transient} resolves itself and must be retried"
        );
    }
    for refusal in [libc::ENOTTY, libc::EPERM, libc::EACCES, libc::EINVAL] {
        assert!(
            !super::console_graph_wait_is_retryable(refusal),
            "errno {refusal} will refuse identically forever and must be reported, \
             not absorbed into a polling cadence"
        );
    }
}
