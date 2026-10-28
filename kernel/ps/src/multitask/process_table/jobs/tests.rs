use super::*;
use crate::multitask::process_table::tests::{isolate_process_table, new_state};

fn spawn_in(pid: u64, parent: Option<u64>, job: Option<JobId>) -> Option<ProcessHandle> {
    let reservation = reserve_spawn_in(job)?;
    let handle = spawn::publish_spawn(reservation, pid, parent, new_state());
    if handle.is_none() {
        cancel_spawn(reservation);
    }
    handle
}

#[test]
fn every_process_starts_in_the_root_job_and_children_inherit_their_parents_job() {
    let _isolation = isolate_process_table();
    let child_job = job_create_child(ROOT_JOB).unwrap();
    spawn_in(10, None, Some(child_job)).unwrap();
    spawn_in(11, Some(10), None).expect("a fork keeps the parent's job");
    spawn_in(12, None, None).expect("no parent and no job means the root job");
    assert_eq!(process_job(10), Some(child_job));
    assert_eq!(process_job(11), Some(child_job));
    assert_eq!(process_job(12), Some(ROOT_JOB));
    let info = job_info(child_job).unwrap();
    assert_eq!((info.processes, info.subtree_processes), (2, 2));
    assert_eq!(job_info(ROOT_JOB).unwrap().subtree_processes, 3);
}

#[test]
fn killing_a_job_refuses_new_members_and_children_in_its_whole_subtree() {
    let _isolation = isolate_process_table();
    let parent = job_create_child(ROOT_JOB).unwrap();
    let leaf = job_create_child(parent).unwrap();
    spawn_in(20, None, Some(leaf)).unwrap();
    job_kill_begin(parent).unwrap();
    assert_eq!(job_info(leaf).unwrap().state, JobState::Killing);
    assert_eq!(job_create_child(leaf), Err(JobError::Killing));
    assert!(spawn_in(21, Some(20), None).is_none(), "a fork racing the kill is refused");
    assert!(reserve_spawn_in(Some(leaf)).is_none());
    let mut pids = [0_u64; 4];
    assert_eq!(job_collect_members(parent, &mut pids), 1);
    assert_eq!(pids[0], 20);
}

#[test]
fn a_dead_job_is_recycled_only_after_its_last_process_object_is_gone() {
    let _isolation = isolate_process_table();
    let job = job_create_child(ROOT_JOB).unwrap();
    let handle = spawn_in(30, None, Some(job)).unwrap();
    job_kill_begin(job).unwrap();
    assert_eq!(job_info(job).unwrap().state, JobState::Killing, "a member object still names it");
    assert!(mark_process_exiting(30).is_some());
    // The exiting process has not been reaped: its object still carries the
    // job index, so recycling the slot now would alias a stale job id.
    let mut pids = [0_u64; 2];
    assert_eq!(job_collect_members(job, &mut pids), 0);
    assert!(job_info(job).is_ok());
    let _ = handle;
}

#[test]
fn stale_ids_and_the_root_job_are_protected() {
    let _isolation = isolate_process_table();
    assert_eq!(job_kill_begin(ROOT_JOB), Err(JobError::Root));
    let job = job_create_child(ROOT_JOB).unwrap();
    job_kill_begin(job).unwrap();
    // No member and no child: the slot was released inside the kill.
    assert_eq!(job_info(job), Err(JobError::Stale));
    let again = job_create_child(ROOT_JOB).unwrap();
    assert_ne!(again, job, "a recycled slot carries a new generation");
    assert_eq!(job_info(job), Err(JobError::Stale));
    assert_eq!(job_info(JobId::from_raw(u64::MAX)), Err(JobError::Stale));
}

#[test]
fn nesting_and_slab_capacity_are_bounded() {
    let _isolation = isolate_process_table();
    let mut job = ROOT_JOB;
    for _ in 0..JOB_MAX_DEPTH {
        job = job_create_child(job).unwrap();
    }
    assert_eq!(job_create_child(job), Err(JobError::TooDeep));
    let mut created = 0;
    while job_create_child(ROOT_JOB).is_ok() {
        created += 1;
        assert!(created <= JOB_SLOTS, "the slab must report Full");
    }
    assert_eq!(job_create_child(ROOT_JOB), Err(JobError::Full));
}

#[test]
fn job_ids_round_trip_through_their_raw_form() {
    let id = JobId { index: 7, generation: 9 };
    assert_eq!(JobId::from_raw(id.to_raw()), id);
    assert_eq!(JobId::from_raw(ROOT_JOB.to_raw()), ROOT_JOB);
}

#[test]
fn subtree_membership_follows_the_ancestor_chain() {
    let _isolation = isolate_process_table();
    let a = job_create_child(ROOT_JOB).unwrap();
    let b = job_create_child(a).unwrap();
    let sibling = job_create_child(ROOT_JOB).unwrap();
    spawn_in(40, None, Some(b)).unwrap();
    assert!(job_subtree_contains_process(a, 40));
    assert!(job_subtree_contains_process(b, 40));
    assert!(!job_subtree_contains_process(sibling, 40));
    assert!(!job_subtree_contains_process(a, 999));
}
