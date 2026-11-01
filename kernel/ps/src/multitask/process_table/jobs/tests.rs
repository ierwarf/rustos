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
fn a_reserved_spawn_revalidates_its_job_at_publication() {
    let _isolation = isolate_process_table();
    let job = job_create_child(ROOT_JOB).unwrap();
    let member = spawn_in(22, None, Some(job)).unwrap();
    let reservation = reserve_spawn_in(Some(job)).unwrap();
    job_kill_begin(job).unwrap();
    assert!(spawn::publish_spawn(reservation, 23, None, new_state()).is_none());
    assert!(cancel_spawn(reservation), "refusal preserves exact rollback custody");
    assert_eq!(process_job(23), None);
    assert_eq!(process_job(22), Some(job));
    let _ = member;
}

#[test]
fn a_reserved_spawn_cannot_join_a_recycled_job_slot() {
    let _isolation = isolate_process_table();
    let job = job_create_child(ROOT_JOB).unwrap();
    let reservation = reserve_spawn_in(Some(job)).unwrap();
    job_kill_begin(job).unwrap();
    let replacement = job_create_child(ROOT_JOB).unwrap();
    assert_eq!(replacement.index, job.index);
    assert_ne!(replacement.generation, job.generation);
    assert!(spawn::publish_spawn(reservation, 24, None, new_state()).is_none());
    assert!(cancel_spawn(reservation));
    assert_eq!(job_info(replacement).unwrap().processes, 0);
    assert_eq!(job_info(replacement).unwrap().state, JobState::Live);
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
    detach_task(handle).expect("detach the last thread");
    assert_eq!(reap_exited_processes(), 1);
    assert!(job_info(job).is_ok(), "reaping does not itself release job slots");
    assert_eq!(job_collect_members(job, &mut pids), 0);
    assert_eq!(job_info(job), Err(JobError::Stale));
    let replacement = job_create_child(ROOT_JOB).unwrap();
    assert_eq!(replacement.index, job.index);
    assert_ne!(replacement.generation, job.generation);
    assert_eq!(job_kill_begin(job), Err(JobError::Stale));
    assert_eq!(job_info(replacement).unwrap().state, JobState::Live);
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

#[test]
fn a_job_is_within_itself_and_its_descendants_but_not_siblings_ancestors_or_stale_ids() {
    let _isolation = isolate_process_table();
    let a = job_create_child(ROOT_JOB).unwrap();
    let b = job_create_child(a).unwrap();
    let sibling = job_create_child(ROOT_JOB).unwrap();
    assert!(job_is_within(a, a));
    assert!(job_is_within(a, b));
    assert!(job_is_within(ROOT_JOB, b));
    assert!(!job_is_within(b, a), "authority never flows upward");
    assert!(!job_is_within(a, sibling));
    // A recycled slot carries a new generation, so the old id is outside every subtree.
    job_kill_begin(sibling).unwrap();
    assert!(!job_is_within(ROOT_JOB, sibling));
    assert!(!job_is_within(ROOT_JOB, JobId::from_raw(u64::MAX)));
}

fn limits(processes: u64, threads: u64) -> JobLimits {
    JobLimits { processes, threads, memory_pages: LIMIT_UNSET }
}

fn memory_limit(pages: u64) -> JobLimits {
    JobLimits { processes: LIMIT_UNSET, threads: LIMIT_UNSET, memory_pages: pages }
}

#[test]
fn a_new_job_has_no_limits_and_its_effective_limit_is_the_tightest_on_its_chain() {
    let _isolation = isolate_process_table();
    let parent = job_create_child(ROOT_JOB).unwrap();
    let child = job_create_child(parent).unwrap();
    assert_eq!(job_info(child).unwrap().limits, JobLimits::UNSET);
    job_set_limits(parent, LimitMask::PROCESSES, limits(3, LIMIT_UNSET)).unwrap();
    job_set_limits(child, LimitMask::PROCESSES, limits(10, LIMIT_UNSET)).unwrap();
    let info = job_info(child).unwrap();
    assert_eq!(info.limits.processes, 10, "a larger child limit is accepted as configured");
    assert_eq!(info.effective.processes, 3, "and clamped by the parent in the effective limit");
    // Tightening the parent later changes the child's effective limit live.
    job_set_limits(parent, LimitMask::PROCESSES, limits(1, LIMIT_UNSET)).unwrap();
    assert_eq!(job_info(child).unwrap().effective.processes, 1);
}

#[test]
fn a_process_limit_refuses_the_next_process_and_counts_the_whole_subtree() {
    let _isolation = isolate_process_table();
    let parent = job_create_child(ROOT_JOB).unwrap();
    let child = job_create_child(parent).unwrap();
    job_set_limits(parent, LimitMask::PROCESSES, limits(2, LIMIT_UNSET)).unwrap();
    assert!(spawn_in(100, None, Some(parent)).is_some());
    assert!(spawn_in(101, None, Some(child)).is_some(), "a descendant's process counts toward the parent");
    assert!(spawn_in(102, None, Some(parent)).is_none(), "the parent's limit of two is reached");
    assert!(spawn_in(103, None, Some(child)).is_none(), "a descendant cannot exceed an ancestor's limit");
    assert!(spawn_in(104, None, None).is_some(), "an unrelated job is unaffected");
    let info = job_info(parent).unwrap();
    assert_eq!((info.usage_processes, info.effective.processes), (2, 2));
}

#[test]
fn reaping_a_process_frees_its_place_under_a_job_limit() {
    let _isolation = isolate_process_table();
    let job = job_create_child(ROOT_JOB).unwrap();
    job_set_limits(job, LimitMask::PROCESSES, limits(1, LIMIT_UNSET)).unwrap();
    let handle = spawn_in(110, None, Some(job)).unwrap();
    assert!(spawn_in(111, None, Some(job)).is_none());
    detach_task(handle).unwrap();
    // Exiting but unreaped objects still hold their place.
    assert!(spawn_in(112, None, Some(job)).is_none());
    assert_eq!(reap_exited_processes(), 1);
    assert!(spawn_in(113, None, Some(job)).is_some());
}

#[test]
fn lowering_a_limit_below_usage_evicts_nobody_and_only_stops_admission() {
    let _isolation = isolate_process_table();
    let job = job_create_child(ROOT_JOB).unwrap();
    for pid in 120..123 {
        spawn_in(pid, None, Some(job)).unwrap();
    }
    job_set_limits(job, LimitMask::PROCESSES, limits(1, LIMIT_UNSET)).unwrap();
    let info = job_info(job).unwrap();
    assert_eq!((info.usage_processes, info.effective.processes), (3, 1));
    assert!(spawn_in(124, None, Some(job)).is_none());
    for pid in 120..123 {
        assert!(process_job(pid).is_some(), "process {pid} still lives");
    }
}

#[test]
fn a_thread_limit_refuses_the_next_thread_across_the_subtree() {
    let _isolation = isolate_process_table();
    let parent = job_create_child(ROOT_JOB).unwrap();
    let child = job_create_child(parent).unwrap();
    // Each process starts with one thread.
    job_set_limits(parent, LimitMask::THREADS, limits(LIMIT_UNSET, 4)).unwrap();
    let a = spawn_in(130, None, Some(parent)).unwrap();
    let b = spawn_in(131, None, Some(child)).unwrap();
    assert_eq!(job_info(parent).unwrap().usage_threads, 2);
    assert!(attach_task(a).is_some());
    assert!(attach_task(b).is_some());
    assert_eq!(job_info(parent).unwrap().usage_threads, 4);
    assert!(attach_task(a).is_none(), "the parent's four threads are used");
    assert!(attach_task(b).is_none(), "a descendant counts against its ancestor");
    assert!(spawn_in(132, None, Some(parent)).is_none(), "a new process needs a thread too");
}

#[test]
fn a_recycled_job_slot_does_not_inherit_the_previous_jobs_limits() {
    let _isolation = isolate_process_table();
    let first = job_create_child(ROOT_JOB).unwrap();
    job_set_limits(first, LimitMask::PROCESSES, limits(0, 0)).unwrap();
    job_kill_begin(first).unwrap();
    let second = job_create_child(ROOT_JOB).unwrap();
    assert_ne!(first, second);
    assert_eq!(job_info(second).unwrap().limits, JobLimits::UNSET);
    assert!(spawn_in(140, None, Some(second)).is_some());
}

#[test]
fn the_root_jobs_process_limit_is_the_system_slot_limit() {
    let _isolation = isolate_process_table();
    assert_eq!(job_info(ROOT_JOB).unwrap().limits.processes, process_limit() as u64);
    job_set_limits(ROOT_JOB, LimitMask::PROCESSES, limits(5, LIMIT_UNSET)).unwrap();
    assert_eq!(process_limit(), 5);
    assert_eq!(job_info(ROOT_JOB).unwrap().effective.processes, 5);
    for pid in 150..155 {
        assert!(spawn_in(pid, None, None).is_some());
    }
    assert!(spawn_in(155, None, None).is_none());
    job_set_limits(ROOT_JOB, LimitMask::PROCESSES, limits(LIMIT_UNSET, LIMIT_UNSET)).unwrap();
    assert_eq!(process_limit(), crate::multitask::slot_dir::SLOT_CEILING, "unset clamps to the ceiling");
    super::super::set_process_limit(super::super::DEFAULT_PROCESS_LIMIT);
}

#[test]
fn limits_cannot_be_set_on_a_stale_or_killing_job_and_masks_are_validated() {
    let _isolation = isolate_process_table();
    let job = job_create_child(ROOT_JOB).unwrap();
    assert_eq!(LimitMask::from_bits(0), None);
    assert!(LimitMask::from_bits(1 << 2).is_some(), "memory pages are a limit now");
    assert_eq!(LimitMask::from_bits(1 << 3), None);
    assert!(LimitMask::from_bits(3).is_some());
    assert_eq!(
        job_set_limits(JobId::from_raw(u64::MAX), LimitMask::PROCESSES, limits(1, 1)),
        Err(JobError::Stale)
    );
    spawn_in(160, None, Some(job)).unwrap();
    job_kill_begin(job).unwrap();
    assert_eq!(job_set_limits(job, LimitMask::PROCESSES, limits(1, 1)), Err(JobError::Killing));
}

#[test]
fn a_forked_child_inherits_its_parents_job_and_is_refused_at_publication_when_that_job_is_full() {
    let _isolation = isolate_process_table();
    let job = job_create_child(ROOT_JOB).unwrap();
    job_set_limits(job, LimitMask::PROCESSES, limits(1, LIMIT_UNSET)).unwrap();
    spawn_in(170, None, Some(job)).unwrap();
    // A fork names no job at reservation, so only the publication-time check
    // can see that the inherited job is full.
    assert!(reserve_spawn_in(None).is_some_and(|reservation| {
        cancel_spawn(reservation);
        true
    }));
    assert!(spawn_in(171, Some(170), None).is_none(), "the child would be the second process in a job limited to one");
    job_set_limits(job, LimitMask::PROCESSES, limits(2, LIMIT_UNSET)).unwrap();
    assert!(spawn_in(172, Some(170), None).is_some());
    assert_eq!(process_job(172), Some(job));
}

#[test]
fn a_memory_limit_is_hierarchical_and_reported_with_its_usage() {
    let _isolation = isolate_process_table();
    let parent = job_create_child(ROOT_JOB).unwrap();
    let child = job_create_child(parent).unwrap();
    job_set_limits(parent, LimitMask::MEMORY, memory_limit(100)).unwrap();
    job_set_limits(child, LimitMask::MEMORY, memory_limit(500)).unwrap();
    let info = job_info(child).unwrap();
    assert_eq!(info.limits.memory_pages, 500);
    assert_eq!(info.effective.memory_pages, 100, "the parent's limit clamps the child");
    let group = child.index() as usize;
    kernel_mm::api::charge::charge(group, 30);
    assert_eq!(job_info(child).unwrap().usage_memory_pages, 30);
    assert_eq!(job_info(parent).unwrap().usage_memory_pages, 30, "a descendant's pages count upward");
    assert!(kernel_mm::api::charge::would_admit(group, 70));
    assert!(!kernel_mm::api::charge::would_admit(group, 71), "the parent's 100 pages bind the child");
    kernel_mm::api::charge::uncharge(group, 30);
}

#[test]
fn a_new_job_has_no_memory_limit_and_a_recycled_group_does_not_keep_the_old_one() {
    let _isolation = isolate_process_table();
    let first = job_create_child(ROOT_JOB).unwrap();
    job_set_limits(first, LimitMask::MEMORY, memory_limit(7)).unwrap();
    job_kill_begin(first).unwrap();
    let second = job_create_child(ROOT_JOB).unwrap();
    assert_eq!(job_info(second).unwrap().limits.memory_pages, LIMIT_UNSET);
    assert_eq!(job_info(second).unwrap().effective.memory_pages, LIMIT_UNSET);
}

#[test]
fn a_dead_job_slot_is_not_reused_while_pages_are_still_charged_to_it() {
    let _isolation = isolate_process_table();
    let job = job_create_child(ROOT_JOB).unwrap();
    let group = job.index() as usize;
    // A process's pages can outlive its process object while the address space
    // retires; the job slot must wait for them.
    kernel_mm::api::charge::charge(group, 4);
    job_kill_begin(job).unwrap();
    assert!(job_info(job).is_ok(), "still allocated: pages are charged to it");
    let mut pids = [0_u64; 1];
    job_collect_members(job, &mut pids);
    assert!(job_info(job).is_ok());
    kernel_mm::api::charge::uncharge(group, 4);
    job_collect_members(job, &mut pids);
    assert_eq!(job_info(job), Err(JobError::Stale), "released once the pages drained");
}


#[test]
fn sessions_are_client_funded_hierarchical_and_refunded_exactly_once() {
    let _isolation = isolate_process_table();
    let parent = job_create_child(ROOT_JOB).unwrap();
    let client = job_create_child(parent).unwrap();
    let unrelated = job_create_child(ROOT_JOB).unwrap();
    assert!(spawn_in(18891, None, Some(client)).is_some());
    assert!(spawn_in(18892, None, Some(unrelated)).is_some());
    job_set_session_limit(parent, 2).unwrap();
    job_set_session_limit(client, 99).unwrap();
    assert_eq!(job_session_info(client).unwrap().effective_limit, 2);
    assert_eq!(job_charge_client_session(18891, client).unwrap().usage, 1);
    assert_eq!(job_charge_client_session(18891, client).unwrap().usage, 2);
    assert_eq!(job_charge_client_session(18891, client), Err(JobError::Full));
    assert_eq!(job_session_info(parent).unwrap().usage, 2);
    assert_eq!(job_session_info(ROOT_JOB).unwrap().usage, 2);
    assert_eq!(job_charge_client_session(18891, unrelated), Err(JobError::Stale));
    assert_eq!(job_charge_client_session(18892, unrelated).unwrap().usage, 1);
    assert_eq!(job_session_info(parent).unwrap().usage, 2);
    assert_eq!(job_release_client_session(client).unwrap().usage, 1);
    assert_eq!(job_release_client_session(client).unwrap().usage, 0);
    assert_eq!(job_release_client_session(client), Err(JobError::Stale));
    assert_eq!(job_session_info(ROOT_JOB).unwrap().usage, 1);
    assert_eq!(job_release_client_session(unrelated).unwrap().usage, 0);
}

#[test]
fn killing_job_cannot_recycle_until_session_refunded() {
    let _isolation = isolate_process_table();
    let client = job_create_child(ROOT_JOB).unwrap();
    spawn_in(18893, None, Some(client)).unwrap();
    job_charge_client_session(18893, client).unwrap();
    job_kill_begin(client).unwrap();
    assert!(job_session_info(client).is_ok());
    assert_eq!(job_charge_client_session(18893, client), Err(JobError::Killing));
    let mut pids = [0_u64; 4];
    job_collect_members(client, &mut pids);
    assert!(job_session_info(client).is_ok());
    job_release_client_session(client).unwrap();
    // The process object may still hold the Job after its session is gone.
    assert_eq!(job_session_info(client).unwrap().usage, 0);
}

#[test]
fn roots_are_bound_to_their_job_at_publication_and_at_exec_commit() {
    // The host ledger is not installed, so the binding itself is a source
    // witness: it must follow the object becoming visible, and exec must bind
    // the new image's root rather than leave it unbound.
    let spawn = include_str!("../spawn.rs");
    let publish = spawn.split_once("fn publish_inner(").unwrap().1;
    let object = publish.find("slot.object = Some(object);").unwrap();
    let bind = publish.find("kernel_mm::api::phys::bind_root_charge(root_phys, job_index)").unwrap();
    let root = publish.find("let root_phys = object.mm_execution.root_phys();").unwrap();
    assert!(root < object && object < bind, "read the root before the move, bind once published");
    let table = include_str!("../../process_table.rs");
    let exec = table.split_once("object.mm_execution = new_mm.pin_for_publication();").unwrap().1;
    assert!(exec[..600].contains("bind_root_charge(new_root, object.job.index() as usize)"));
}

/// Exhaustive kill/reap interleavings.
///
/// One job `J` (child of the root) with one live-session slot is driven through
/// every sequence of up to `DEPTH` operations drawn from the alphabet below,
/// on a freshly reset table each time. After every operation the table is
/// compared with a straight-line model, so the property is not "these cases
/// pass" but "no ordering of spawn / kill / exit / detach / reap / session
/// charge / release / child creation breaks these invariants":
///
/// - a Killing job admits no member, child job or session;
/// - the job slot is recycled (its id goes Stale) only once every process
///   object in it is reaped, no child remains and no session is outstanding;
/// - until then `usage_processes` equals the model's unreaped object count and
///   `JobInfo::processes` counts only live members;
/// - a recycled slot never aliases the old id: every later operation on the old
///   id is Stale and a fresh child of the root carries a new generation.
mod kill_reap_interleavings {
    use super::*;
    use alloc::vec::Vec;

    const DEPTH: usize = 5;
    const OPS: u8 = 9;

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Proc {
        Live,
        /// Marked exiting but still holding its thread.
        Exiting,
        /// Last thread detached: reapable.
        Detached,
    }

    struct Model {
        job: JobId,
        killing: bool,
        stale: bool,
        procs: Vec<(u64, ProcHandleBox, Proc)>,
        sessions: u64,
        children: u64,
        next_pid: u64,
    }

    struct ProcHandleBox(ProcessHandle);

    impl Model {
        fn unreaped(&self) -> u64 {
            self.procs.len() as u64
        }
        fn live(&self) -> u64 {
            self.procs.iter().filter(|(_, _, state)| *state == Proc::Live).count() as u64
        }
        /// The slot's death condition: killing, empty, childless, no sessions.
        fn must_be_recycled(&self) -> bool {
            self.killing && self.procs.is_empty() && self.children == 0 && self.sessions == 0
        }
    }

    fn apply(model: &mut Model, op: u8) {
        let job = model.job;
        match op {
            // Spawn a member.
            0 => {
                let spawned = spawn_in(model.next_pid, None, Some(job));
                match spawned {
                    Some(handle) => {
                        assert!(!model.killing && !model.stale, "a dying job admitted a member");
                        model.procs.push((model.next_pid, ProcHandleBox(handle), Proc::Live));
                        model.next_pid += 1;
                    }
                    None => assert!(
                        model.killing || model.stale,
                        "a live job refused a member under no limit"
                    ),
                }
            }
            1 => {
                let result = job_kill_begin(job);
                if model.stale {
                    assert_eq!(result, Err(JobError::Stale));
                } else {
                    assert!(result.is_ok() || model.killing);
                    model.killing = true;
                    // Kill covers the subtree; the model's child jobs never
                    // hold members, so they die and are reclaimed at once.
                    model.children = 0;
                }
            }
            // Mark the oldest live member exiting.
            2 => {
                if let Some((pid, _, state)) =
                    model.procs.iter_mut().find(|(_, _, state)| *state == Proc::Live)
                {
                    assert!(mark_process_exiting(*pid).is_some());
                    *state = Proc::Exiting;
                }
            }
            // Detach the last thread of the oldest not-yet-detached member.
            3 => {
                if let Some((_, handle, state)) =
                    model.procs.iter_mut().find(|(_, _, state)| *state != Proc::Detached)
                {
                    detach_task(handle.0).expect("detach the only thread");
                    *state = Proc::Detached;
                }
            }
            4 => {
                let reaped = reap_exited_processes() as usize;
                let before = model.procs.len();
                model.procs.retain(|(_, _, state)| *state != Proc::Detached);
                assert_eq!(reaped, before - model.procs.len(), "reap count disagrees with model");
            }
            5 => {
                let child = job_create_child(job);
                match child {
                    Ok(_) => {
                        assert!(!model.killing && !model.stale, "a dying job admitted a child");
                        model.children += 1;
                    }
                    Err(JobError::Killing) => assert!(model.killing),
                    Err(JobError::Stale) => assert!(model.stale),
                    Err(other) => panic!("unexpected child creation error {other:?}"),
                }
            }
            // Charge a session for the oldest live member.
            6 => {
                let Some((pid, _, _)) = model.procs.iter().find(|(_, _, s)| *s == Proc::Live)
                else {
                    return;
                };
                match job_charge_client_session(*pid, job) {
                    Ok(_) => {
                        assert!(!model.killing && !model.stale, "a dying job admitted a session");
                        model.sessions += 1;
                    }
                    Err(JobError::Killing) => assert!(model.killing),
                    Err(JobError::Stale) => assert!(model.stale),
                    Err(other) => panic!("unexpected session error {other:?}"),
                }
            }
            7 => {
                let released = job_release_client_session(job);
                match released {
                    Ok(_) => {
                        assert!(model.sessions > 0);
                        model.sessions -= 1;
                    }
                    Err(JobError::Stale) => assert!(model.sessions == 0 || model.stale),
                    Err(other) => panic!("unexpected release error {other:?}"),
                }
            }
            // Slow recovery path: collect (and thereby release dead) jobs.
            8 => {
                let mut pids = [0_u64; 8];
                let collected = job_collect_members(job, &mut pids) as u64;
                if !model.stale {
                    assert!(collected <= model.live() + model.procs.len() as u64);
                }
            }
            _ => unreachable!(),
        }
    }

    /// The model cannot see when recycling happens (it is driven by reap and
    /// collect), so both outcomes are legal until the death condition holds;
    /// once it does, the very next table-visiting call must have recycled it.
    fn check(model: &mut Model, trace: &[u8]) {
        let info = job_info(model.job);
        if info.is_err() {
            assert_eq!(info, Err(JobError::Stale));
            assert!(
                model.stale || model.must_be_recycled(),
                "job recycled while it still had members/children/sessions: {trace:?} {:?}",
                (model.killing, model.procs.len(), model.children, model.sessions)
            );
            model.stale = true;
        } else {
            let info = info.unwrap();
            assert!(!model.stale, "a recycled id came back to life: {trace:?}");
            assert_eq!(info.usage_processes, model.unreaped(), "usage drifted: {trace:?}");
            assert!(info.processes <= model.unreaped());
            let sessions = job_session_info(model.job).expect("live job has session info").usage;
            assert_eq!(sessions, model.sessions, "session count drifted: {trace:?}");
            assert_eq!(info.state == JobState::Killing, model.killing, "kill state: {trace:?}");
        }
    }

    fn run(trace: &[u8]) {
        let _isolation = isolate_process_table();
        let job = job_create_child(ROOT_JOB).unwrap();
        let mut model = Model {
            job,
            killing: false,
            stale: false,
            procs: Vec::new(),
            sessions: 0,
            children: 0,
            next_pid: 40_000,
        };
        for (step, &op) in trace.iter().enumerate() {
            apply(&mut model, op);
            check(&mut model, &trace[..=step]);
        }
        // Liveness: a dead job must be reclaimed by one visit of the slow
        // recovery path, never left pinned by an interleaving.
        apply(&mut model, 8);
        check(&mut model, trace);
        if model.must_be_recycled() {
            assert!(model.stale, "a dead job was not reclaimed: {trace:?}");
        }
        if model.stale {
            // The old id must stay dead and the slot must come back with a new
            // generation rather than alias it.
            assert_eq!(job_kill_begin(job), Err(JobError::Stale));
            assert_eq!(spawn_in(99_999, None, Some(job)), None);
            let fresh = job_create_child(ROOT_JOB).unwrap();
            if fresh.index == job.index {
                assert_ne!(fresh.generation, job.generation);
            }
        }
    }

    fn enumerate(prefix: &mut Vec<u8>, count: &mut u64) {
        if !prefix.is_empty() {
            run(prefix);
            *count += 1;
        }
        if prefix.len() == DEPTH {
            return;
        }
        for op in 0..OPS {
            prefix.push(op);
            enumerate(prefix, count);
            prefix.pop();
        }
    }

    #[test]
    fn no_ordering_of_kill_exit_detach_reap_and_session_ops_breaks_job_lifetime() {
        let mut count = 0;
        enumerate(&mut Vec::new(), &mut count);
        assert_eq!(count, (1..=DEPTH as u32).map(|n| u64::from(OPS).pow(n)).sum::<u64>());
    }
}

mod provider_incarnation {
    use super::*;

    /// Provider `pid` exits and is reaped; a new process then reuses the PID.
    fn replace_provider(pid: u64, handle: ProcessHandle) -> ProcessHandle {
        mark_process_exiting(pid).expect("provider exits");
        detach_task(handle).expect("detach provider thread");
        assert_eq!(reap_exited_processes(), 1);
        spawn_in(pid, None, None).expect("a new process reuses the PID")
    }

    #[test]
    fn tokens_are_idempotent_for_their_own_incarnation_and_fenced_by_the_watermark() {
        let _isolation = isolate_process_table();
        let client_job = job_create_child(ROOT_JOB).unwrap();
        spawn_in(7101, None, Some(client_job)).unwrap();
        spawn_in(7100, None, None).unwrap();
        let first = job_charge_client_session_token(7100, 1, 7101, client_job).unwrap();
        assert_eq!(first.usage, 1);
        // A retried CHARGE of the same token returns the same charge.
        assert_eq!(job_charge_client_session_token(7100, 1, 7101, client_job).unwrap().usage, 1);
        // Another client or Job under the same token is a conflict.
        assert_eq!(
            job_charge_client_session_token(7100, 1, 7102, client_job),
            Err(JobError::Stale)
        );
        assert_eq!(job_release_client_session_token(7100, 1, Some(client_job)).unwrap().unwrap().usage, 0);
        // The released nonce is below the watermark and can never bill again.
        assert_eq!(
            job_charge_client_session_token(7100, 1, 7101, client_job),
            Err(JobError::Stale)
        );
        assert_eq!(job_charge_client_session_token(7100, 2, 7101, client_job).unwrap().usage, 1);
    }

    #[test]
    fn a_reused_provider_pid_cannot_replay_or_collide_with_the_previous_incarnations_tokens() {
        let _isolation = isolate_process_table();
        let client_job = job_create_child(ROOT_JOB).unwrap();
        spawn_in(7201, None, Some(client_job)).unwrap();
        let old = spawn_in(7200, None, None).unwrap();
        job_charge_client_session_token(7200, 1, 7201, client_job).unwrap();
        // The provider dies before rootd drains its ledger; its PID is reused.
        let _new = replace_provider(7200, old);
        // The token is not the new incarnation's: neither an idempotent retry
        // nor a fresh nonce above the old watermark may touch the old ledger.
        assert_eq!(
            job_charge_client_session_token(7200, 1, 7201, client_job),
            Err(JobError::Stale),
            "an old token must not be adopted by the new incarnation"
        );
        assert_eq!(
            job_charge_client_session_token(7200, 9, 7201, client_job),
            Err(JobError::Stale),
            "a new incarnation waits for the old ledger to drain"
        );
        assert_eq!(job_session_info(client_job).unwrap().usage, 1, "the old charge still stands");
        // Provider-exit reconciliation drains the old incarnation exactly once.
        assert_eq!(job_drain_client_session_tokens(Some(7200)), Ok(1));
        assert_eq!(job_session_info(client_job).unwrap().usage, 0);
        // The new incarnation now starts a clean ledger, including nonce 1.
        assert_eq!(job_charge_client_session_token(7200, 1, 7201, client_job).unwrap().usage, 1);
    }

    #[test]
    fn a_tombstone_before_any_charge_does_not_block_the_live_incarnation() {
        let _isolation = isolate_process_table();
        let client_job = job_create_child(ROOT_JOB).unwrap();
        spawn_in(7301, None, Some(client_job)).unwrap();
        spawn_in(7300, None, None).unwrap();
        // A cancel of a never-charged token fences nonce 3 for this provider.
        assert_eq!(job_release_client_session_token(7300, 3, None), Ok(None));
        assert_eq!(
            job_charge_client_session_token(7300, 3, 7301, client_job),
            Err(JobError::Stale)
        );
        assert_eq!(job_charge_client_session_token(7300, 4, 7301, client_job).unwrap().usage, 1);
    }
}
