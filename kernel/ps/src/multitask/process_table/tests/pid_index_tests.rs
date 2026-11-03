//! pid -> slot index: maintained by publish/reclaim, never trusted without
//! re-validating the slot's own object.

use super::super::{PID_BUCKETS, PROCESS_TABLE, pid_bucket};
use super::{
    attach_task, create_process, detach_task, is_process_exiting, isolate_process_table,
    mark_process_exiting, new_state, reap_exited_processes,
};
use crate::multitask::process_table::{
    live_process_identity_by_pid, mark_process_exiting_once, note_process_exit_status, parent_process_id_of,
    retain_process_by_pid, thread_count_by_pid,
};

/// Every occupied slot is on exactly its own chain, and every chain entry is an
/// occupied slot of that bucket.
fn assert_index_matches_slots() {
    let table = PROCESS_TABLE.lock();
    let mut linked = 0usize;
    for bucket in 0..PID_BUCKETS {
        let mut cursor = table.pid_buckets[bucket];
        while cursor != 0 {
            let slot = &table.slots[cursor as usize - 1];
            let object = slot.object.as_deref().expect("chain entry names an empty slot");
            assert_eq!(pid_bucket(object.process_id), bucket);
            linked += 1;
            cursor = slot.pid_next;
            assert!(linked <= table.slots.len(), "chain cycle");
        }
    }
    let occupied = table.slots.iter().filter(|slot| slot.object.is_some()).count();
    assert_eq!(linked, occupied);
}

fn colliding_pid(base: u64) -> u64 {
    (base + 1..).find(|pid| pid_bucket(*pid) == pid_bucket(base)).unwrap()
}

#[test]
fn pid_lookups_hit_publish_and_miss_after_reap() {
    let _isolation = isolate_process_table();
    let handle = create_process(91_001, new_state()).expect("process");
    assert_index_matches_slots();
    assert_eq!(is_process_exiting(91_001), Some(false));
    assert_eq!(thread_count_by_pid(91_001), Some(1), "creation holds the first thread");
    assert_eq!(parent_process_id_of(91_001), None);
    assert!(live_process_identity_by_pid(91_001).is_some());
    assert!(is_process_exiting(91_002).is_none());

    attach_task(handle).expect("thread");
    assert_eq!(thread_count_by_pid(91_001), Some(2));
    detach_task(handle).expect("second thread");
    assert_eq!(mark_process_exiting(91_001), Some(()));
    detach_task(handle).expect("retire");
    assert_eq!(reap_exited_processes(), 1);
    assert_index_matches_slots();
    assert!(is_process_exiting(91_001).is_none());
    assert!(thread_count_by_pid(91_001).is_none());
    assert!(live_process_identity_by_pid(91_001).is_none());
    assert!(retain_process_by_pid(91_001).is_none());
}

#[test]
fn colliding_pids_stay_distinct_and_unlink_from_any_chain_position() {
    let _isolation = isolate_process_table();
    let a = 92_000;
    let b = colliding_pid(a);
    let c = colliding_pid(b);
    let handles = [a, b, c].map(|pid| create_process(pid, new_state()).expect("process"));
    assert_index_matches_slots();
    for (pid, handle) in [a, b, c].into_iter().zip(handles) {
        let identity = live_process_identity_by_pid(pid).expect("indexed");
        assert_eq!(identity.process_id(), pid);
        assert_eq!(identity.process_generation(), handle.generation());
    }
    // Middle, head (last linked = c) and tail removal in turn.
    let pids = [a, b, c];
    let order = [1usize, 2, 0];
    for (step, victim) in order.into_iter().enumerate() {
        let handle = handles[victim];
        assert_eq!(mark_process_exiting(pids[victim]), Some(()));
        detach_task(handle).expect("retire");
        assert_eq!(reap_exited_processes(), 1);
        assert_index_matches_slots();
        assert!(is_process_exiting(pids[victim]).is_none());
        for survivor in &order[step + 1..] {
            assert!(
                is_process_exiting(pids[*survivor]).is_some(),
                "survivor lost from the chain"
            );
        }
    }
}

#[test]
fn reused_pid_resolves_to_the_new_incarnation_only() {
    let _isolation = isolate_process_table();
    let old = create_process(93_001, new_state()).expect("first");
    assert_eq!(mark_process_exiting(93_001), Some(()));
    detach_task(old).expect("retire");
    assert_eq!(reap_exited_processes(), 1);
    let new = create_process(93_001, new_state()).expect("reuse pid");
    assert_eq!(new.index(), old.index());
    assert_ne!(new.generation(), old.generation());
    assert_index_matches_slots();
    let identity = live_process_identity_by_pid(93_001).expect("new incarnation");
    assert_eq!(identity.process_generation(), new.generation());
    let reference = retain_process_by_pid(93_001).expect("ref");
    assert_eq!(reference.handle().generation(), new.generation());
}

#[test]
fn exit_marking_is_exactly_once_through_the_index_and_status_sticks() {
    let _isolation = isolate_process_table();
    let handle = create_process(94_001, new_state()).expect("process");
    assert_eq!(mark_process_exiting_once(94_001), Some(true));
    assert_eq!(mark_process_exiting_once(94_001), Some(false));
    assert_eq!(is_process_exiting(94_001), Some(true));
    assert!(live_process_identity_by_pid(94_001).is_none(), "exiting is not live");
    assert_eq!(note_process_exit_status(94_001, 7), Some(()));
    assert!(note_process_exit_status(94_999, 7).is_none());
    detach_task(handle).expect("retire");
    assert_eq!(reap_exited_processes(), 1);
    assert_index_matches_slots();
    assert!(is_process_exiting(94_001).is_none());
}
