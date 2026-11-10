//! Launching programs from the home screen.
//!
//! A click starts one background request. The request path is bounded and
//! never ends the compositor: a burst of clicks cannot exhaust threads, a
//! program that is already being requested is not requested again, and a
//! thread that cannot be spawned drops that one request.

use std::sync::{Mutex, PoisonError};

use runtime_control::{MutationOutcome, RuntimeClient, RuntimeTransaction};

use crate::sys::{self, diag_line};

/// Launch requests that may be in flight at once.
const MAX_LAUNCHES_IN_FLIGHT: usize = 4;

/// Tracks which programs have a launch request in flight.
pub(super) struct LaunchGate {
    in_flight: Mutex<Vec<String>>,
    capacity: usize,
}

impl LaunchGate {
    pub(super) const fn new() -> Self {
        Self::with_capacity(MAX_LAUNCHES_IN_FLIGHT)
    }

    pub(super) const fn with_capacity(capacity: usize) -> Self {
        Self {
            in_flight: Mutex::new(Vec::new()),
            capacity,
        }
    }

    /// Reserve a slot for `program`. Fails when the program is already being
    /// launched or all slots are taken.
    pub(super) fn try_begin(&self, program: &str) -> bool {
        let mut in_flight = self
            .in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if in_flight.len() >= self.capacity || in_flight.iter().any(|id| id == program) {
            return false;
        }
        in_flight.push(program.to_owned());
        true
    }

    /// Release the slot taken by `try_begin`.
    pub(super) fn finish(&self, program: &str) {
        let mut in_flight = self
            .in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(index) = in_flight.iter().position(|id| id == program) {
            in_flight.swap_remove(index);
        }
    }
}

static LAUNCH_GATE: LaunchGate = LaunchGate::new();

const MUTATION_QUERY_BACKOFF: std::time::Duration = std::time::Duration::from_millis(200);

#[derive(Debug, Eq, PartialEq)]
enum MutationCompletion {
    Committed,
    Rejected(i32),
    Unknown(i32),
}

/// Reconcile only the original identity. A transport failure is never a
/// reason to submit another mutation.
fn reconcile_mutation(
    first: MutationOutcome,
    mut query: impl FnMut() -> MutationOutcome,
    mut remaining: impl FnMut() -> Result<std::time::Duration, i32>,
    mut sleep: impl FnMut(std::time::Duration),
) -> MutationCompletion {
    let mut outcome = first;
    loop {
        match outcome {
            MutationOutcome::Committed => return MutationCompletion::Committed,
            MutationOutcome::Rejected(err) => return MutationCompletion::Rejected(err),
            MutationOutcome::Unknown(err) => return MutationCompletion::Unknown(err),
            MutationOutcome::Pending | MutationOutcome::Transport(_) => {}
        }
        let delay = match remaining() {
            Ok(remaining) if !remaining.is_zero() => remaining.min(MUTATION_QUERY_BACKOFF),
            Ok(_) => return MutationCompletion::Unknown(libc::ESTALE),
            Err(err) => return MutationCompletion::Unknown(err),
        };
        sleep(delay);
        if let Err(err) = remaining() {
            return MutationCompletion::Unknown(err);
        }
        outcome = query();
    }
}

fn execute_mutation(
    runtime: &RuntimeClient,
    transaction: &RuntimeTransaction,
) -> MutationCompletion {
    reconcile_mutation(
        runtime.submit_transaction(transaction),
        || runtime.query_transaction(transaction),
        || transaction.query_remaining(),
        std::thread::sleep,
    )
}

fn report_mutation_result(action: &str, result: MutationCompletion) {
    match result {
        MutationCompletion::Committed => {}
        MutationCompletion::Rejected(errno) => {
            diag_line(format!("uiserver: {action} rejected errno={errno}"))
        }
        MutationCompletion::Unknown(errno) => diag_line(format!(
            "uiserver: {action} outcome unknown errno={errno}; no automatic retry"
        )),
    }
}

pub(super) fn launch_program_async(desktop_file_id: String) {
    if !LAUNCH_GATE.try_begin(&desktop_file_id) {
        diag_line("uiserver: launch request ignored: already launching or too many in flight");
        return;
    }
    let for_thread = desktop_file_id.clone();
    let spawned = sys::spawn_ui_thread(
        sys::UiThreadRole::Background,
        "uiserver-program-launch",
        move || {
            match RuntimeClient::open_default().and_then(|runtime| {
                let transaction = runtime.launch_transaction(for_thread.as_str())?;
                Ok(execute_mutation(&runtime, &transaction))
            }) {
                Ok(result) => report_mutation_result("launch", result),
                Err(errno) => diag_line(format!(
                    "uiserver: launch could not be prepared errno={errno}"
                )),
            }
            LAUNCH_GATE.finish(for_thread.as_str());
        },
    );
    if spawned.is_err() {
        // Losing one launch is recoverable; ending the compositor is not.
        LAUNCH_GATE.finish(&desktop_file_id);
        diag_line("uiserver: launch thread could not be spawned; request dropped");
    }
}

/// Terminate requests that may be in flight at once. Console sessions are
/// capped far below this, so a request is only refused for a handle that is
/// already being terminated.
const MAX_TERMINATES_IN_FLIGHT: usize = 16;

static TERMINATE_GATE: LaunchGate = LaunchGate::with_capacity(MAX_TERMINATES_IN_FLIGHT);

/// Ask runtimed to end a console session without blocking the UI thread. The
/// RPC may wait out its full I/O deadline, which exceeds the UI phase
/// watchdog; the window is already gone from the UI by the time this runs.
pub(super) fn terminate_session_async(session_handle: u64) {
    let key = format!("terminate:{session_handle}");
    if !TERMINATE_GATE.try_begin(&key) {
        diag_line("uiserver: terminate request ignored: already terminating or too many in flight");
        return;
    }
    let for_thread = key.clone();
    let spawned = sys::spawn_ui_thread(
        sys::UiThreadRole::Background,
        "uiserver-session-terminate",
        move || {
            match RuntimeClient::open_default().and_then(|runtime| {
                let transaction = runtime.terminate_session_transaction(session_handle)?;
                Ok(execute_mutation(&runtime, &transaction))
            }) {
                Ok(result) => report_mutation_result(&format!("terminate session {session_handle}"), result),
                Err(errno) => diag_line(format!("uiserver: terminate session {session_handle} could not be prepared errno={errno}")),
            }
            TERMINATE_GATE.finish(for_thread.as_str());
        },
    );
    if spawned.is_err() {
        TERMINATE_GATE.finish(&key);
        diag_line("uiserver: terminate thread could not be spawned; request dropped");
    }
}

#[cfg(test)]
mod tests {
    use super::{
        execute_mutation, reconcile_mutation, LaunchGate, MutationCompletion,
        MAX_LAUNCHES_IN_FLIGHT,
    };
    use runtime_control::protocol::{
        RuntimeRequest, RuntimeResponse, PROTOCOL_VERSION, REQUEST_FLAG_QUERY,
    };
    use runtime_control::{MutationOutcome, RuntimeClient};
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::time::Duration;

    fn scripted_runtime(
        name: &str,
        statuses: Vec<Option<(u16, i32)>>,
    ) -> (RuntimeClient, std::thread::JoinHandle<Vec<RuntimeRequest>>) {
        let path = std::env::temp_dir().join(format!("uiserver-rpc-{name}-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let client = RuntimeClient::open(path.to_str().unwrap()).unwrap();
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for status in statuses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = RuntimeRequest::default();
                let bytes = unsafe {
                    std::slice::from_raw_parts_mut(
                        (&mut request as *mut RuntimeRequest).cast::<u8>(),
                        std::mem::size_of::<RuntimeRequest>(),
                    )
                };
                stream.read_exact(bytes).unwrap();
                requests.push(request);
                if let Some((op, status)) = status {
                    let response = RuntimeResponse {
                        version: PROTOCOL_VERSION,
                        op: if op == u16::MAX { request.op } else { op },
                        status,
                        count: 0,
                    };
                    let bytes = unsafe {
                        std::slice::from_raw_parts(
                            (&response as *const RuntimeResponse).cast::<u8>(),
                            std::mem::size_of::<RuntimeResponse>(),
                        )
                    };
                    stream.write_all(bytes).unwrap();
                }
            }
            std::fs::remove_file(path).unwrap();
            requests
        });
        (client, server)
    }

    fn assert_one_identity(requests: &[RuntimeRequest]) {
        assert_eq!(requests[0].flags, 0);
        for request in &requests[1..] {
            assert_eq!(request.flags, REQUEST_FLAG_QUERY);
            assert_eq!(request.transaction_id_hi, requests[0].transaction_id_hi);
            assert_eq!(request.transaction_id_lo, requests[0].transaction_id_lo);
            assert_eq!(
                request.transaction_deadline_ns,
                requests[0].transaction_deadline_ns
            );
        }
    }

    #[test]
    fn real_transport_loss_and_pending_reconcile_one_action_to_terminal_success() {
        let (runtime, server) = scripted_runtime(
            "commit",
            vec![
                None,
                Some((u16::MAX, -libc::EINPROGRESS)),
                Some((u16::MAX, -libc::EALREADY)),
                Some((u16::MAX, 0)),
            ],
        );
        let gate = LaunchGate::new();
        assert!(gate.try_begin("app.desktop"));
        let transaction = runtime.launch_transaction("app.desktop").unwrap();
        assert_eq!(
            execute_mutation(&runtime, &transaction),
            MutationCompletion::Committed
        );
        assert!(!gate.try_begin("app.desktop"));
        gate.finish("app.desktop");
        assert!(gate.try_begin("app.desktop"));
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 4);
        assert_one_identity(&requests);
    }

    #[test]
    fn server_restart_unknown_never_resubmits_the_action() {
        let (runtime, server) = scripted_runtime("stale", vec![None, Some((0, -libc::ESTALE))]);
        let transaction = runtime.launch_transaction("app.desktop").unwrap();
        assert_eq!(
            execute_mutation(&runtime, &transaction),
            MutationCompletion::Unknown(libc::ESTALE)
        );
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2);
        assert_one_identity(&requests);
    }

    #[test]
    fn transport_loss_only_queries_original_intent_until_commit() {
        let mut queries = 0;
        let outcome = reconcile_mutation(
            MutationOutcome::Transport(libc::EPIPE),
            || {
                queries += 1;
                if queries < 3 {
                    MutationOutcome::Pending
                } else {
                    MutationOutcome::Committed
                }
            },
            || Ok(Duration::from_secs(1)),
            |_| {},
        );
        assert_eq!(outcome, MutationCompletion::Committed);
        assert_eq!(queries, 3);
    }

    #[test]
    fn repeated_transport_loss_expires_without_another_submit() {
        let mut queries = 0;
        let mut remaining_checks = 0;
        let outcome = reconcile_mutation(
            MutationOutcome::Transport(libc::EIO),
            || {
                queries += 1;
                MutationOutcome::Transport(libc::EIO)
            },
            || {
                remaining_checks += 1;
                if remaining_checks > 4 {
                    Err(libc::ESTALE)
                } else {
                    Ok(Duration::from_secs(1))
                }
            },
            |_| {},
        );
        assert_eq!(outcome, MutationCompletion::Unknown(libc::ESTALE));
        assert_eq!(queries, 2);
    }

    #[test]
    fn rejection_and_unknown_are_not_treated_as_success_or_queried_again() {
        for (input, expected) in [
            (
                MutationOutcome::Rejected(libc::EINVAL),
                MutationCompletion::Rejected(libc::EINVAL),
            ),
            (
                MutationOutcome::Unknown(libc::EOWNERDEAD),
                MutationCompletion::Unknown(libc::EOWNERDEAD),
            ),
        ] {
            let outcome = reconcile_mutation(
                input,
                || panic!("terminal result queried"),
                || panic!("terminal result sleeps"),
                |_| panic!("terminal result sleeps"),
            );
            assert_eq!(outcome, expected);
        }
    }

    #[test]
    fn a_program_already_launching_is_not_launched_again() {
        let gate = LaunchGate::new();
        assert!(gate.try_begin("shell.desktop"));
        assert!(!gate.try_begin("shell.desktop"));
        gate.finish("shell.desktop");
        assert!(
            gate.try_begin("shell.desktop"),
            "a finished launch frees its slot"
        );
    }

    #[test]
    fn launches_in_flight_are_bounded() {
        let gate = LaunchGate::new();
        for index in 0..MAX_LAUNCHES_IN_FLIGHT {
            assert!(gate.try_begin(&format!("program-{index}.desktop")));
        }
        assert!(!gate.try_begin("one-too-many.desktop"));
        gate.finish("program-0.desktop");
        assert!(gate.try_begin("one-too-many.desktop"));
    }

    #[test]
    fn finishing_an_unknown_program_is_harmless() {
        let gate = LaunchGate::new();
        gate.finish("never-started.desktop");
        assert!(gate.try_begin("a.desktop"));
    }
}
