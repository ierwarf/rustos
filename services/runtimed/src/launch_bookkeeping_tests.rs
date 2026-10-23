use super::*;
use crate::launch_worker::{LaunchCalls, LaunchProgress};
use std::thread;

fn entry() -> LaunchEntry {
    LaunchEntry {
        package_id: String::from("pkg"),
        desktop_file_id: String::from("app.desktop"),
        display_name: String::from("App"),
        exec: String::from("app.elf"),
        runtime_deps: Vec::new(),
        restart: false,
        weight_micros: 100,
        logical_admin: false,
        console_hosted: false,
        args: Vec::new(),
        env: Vec::new(),
        private_smp_qualification: None,
    }
}

fn state_with_worker() -> BrokerState {
    BrokerState {
        launch_worker: crate::launch_worker::start(LaunchCalls {
            spawn: |_, _| Ok(41),
            report_lease: |_, _| Ok(()),
            bind_and_activate: |_, _| panic!("a timed-out or duplicate child cannot activate"),
            await_endpoint: |_| panic!("an unactivated child has no endpoint"),
            retire: |pid, _| assert_eq!(pid, 41),
        }),
        ..BrokerState::default()
    }
}

fn wait_for_progress(state: &BrokerState) -> LaunchProgress {
    let deadline = Instant::now()
        + Duration::from_millis(IPC_BOOT_CONTROL_HARD_LIMIT_MS)
        + Duration::from_secs(2);
    loop {
        if let Some(progress) = state.launch_worker.as_ref().expect("worker starts").poll() {
            return progress;
        }
        assert!(Instant::now() < deadline, "the worker reports progress");
        thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn timeout_completion_removes_the_exact_record_even_after_a_late_ack() {
    let mut state = state_with_worker();
    begin_tracked_launch(&mut state, entry()).expect("launch begins");
    let LaunchProgress::Spawned { pid, record_ack } = wait_for_progress(&state) else {
        panic!("the suspended child is reported");
    };
    assert_eq!(pid, 41);

    // Stall the broker beyond the real ACK budget, then apply its queued
    // Spawned and Finished messages in their original FIFO order.
    let LaunchProgress::Finished {
        pid: finished_pid,
        result,
    } = wait_for_progress(&state)
    else {
        panic!("the worker times out while the broker is stalled");
    };
    assert_eq!(finished_pid, Some(41));
    assert_eq!(result, Err(libc::ETIMEDOUT));
    record_spawned_launch(&mut state, pid, record_ack);
    assert!(
        state.running.contains_key(&41),
        "the late Spawned was recorded"
    );
    assert_eq!(
        state.launch_in_flight.as_ref().unwrap().recorded_pid,
        Some(41)
    );

    finish_tracked_launch(&mut state, finished_pid, result);
    assert!(
        !state.running.contains_key(&41),
        "no retired child record survives"
    );
    assert!(
        state.launch_in_flight.is_none(),
        "the launch slot is released"
    );
    assert!(state.retry_after.contains_key("app.desktop"));
    assert_eq!(state.launch_failure_counts.get("app.desktop"), Some(&1));
    assert!(!state.launched_once.contains("pkg"));
}

#[test]
fn duplicate_completion_preserves_the_record_the_launch_did_not_insert() {
    let mut state = state_with_worker();
    state.running.insert(
        41,
        RunningProcess {
            pid: 41,
            package_id: String::from("original"),
            desktop_file_id: String::from("original.desktop"),
            display_name: String::from("Original"),
            exec: String::from("original.elf"),
            session_handle: 0,
            restart: false,
            logical_admin: false,
        },
    );
    begin_tracked_launch(&mut state, entry()).expect("launch begins");
    let LaunchProgress::Spawned { pid, record_ack } = wait_for_progress(&state) else {
        panic!("the suspended child is reported");
    };
    record_spawned_launch(&mut state, pid, record_ack);
    assert_eq!(state.launch_in_flight.as_ref().unwrap().recorded_pid, None);
    let LaunchProgress::Finished { pid, result } = wait_for_progress(&state) else {
        panic!("the duplicate is retired");
    };
    assert_eq!(pid, Some(41));
    assert_eq!(result, Err(libc::EEXIST));
    finish_tracked_launch(&mut state, pid, result);
    assert_eq!(state.running.get(&41).unwrap().package_id, "original");
    assert!(state.launch_in_flight.is_none());
}
