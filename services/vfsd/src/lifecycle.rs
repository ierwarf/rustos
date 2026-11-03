//! Per-process cwd lifecycle: inheritance on fork and pruning on exit.
//!
//! vfsd keys cwd by pid but cannot observe process lifetime itself, so the
//! kernel fans lifecycle evidence out to a private vfsd queue
//! (`LifecycleConsumer::VfsPolicy`, drained with
//! `SYS_RUSTOS_LIFECYCLE_DRAIN_BROKER`). Generation safety comes from order:
//! the queue is FIFO, an exit is queued before its pid can be reused, a fork is
//! queued before the child runs, and vfsd drains the whole queue before it
//! interprets each request. A stale exit therefore can never be applied to a
//! later process that reuses the pid, and a fork event always replaces whatever
//! the child pid's entry held. An overflowed queue means identity evidence was
//! lost, so the caller must forget every cwd (`clear`) rather than guess.

use alloc::collections::BTreeMap;
use alloc::string::String;

use rustos_user_abi::syscall::{LifecycleEventWire, LIFECYCLE_EVENT_EXIT, LIFECYCLE_EVENT_FORK};

/// Applies one drained event to the pid-indexed cwd map.
pub fn apply_lifecycle_event(cwd: &mut BTreeMap<u64, String>, event: &LifecycleEventWire) {
    if event.pid == 0 {
        return;
    }
    match event.event {
        LIFECYCLE_EVENT_EXIT => {
            cwd.remove(&event.pid);
        }
        LIFECYCLE_EVENT_FORK => {
            // POSIX: the child starts in the parent's cwd. A parent without an
            // entry is at the default root, which is also the absence of an
            // entry, so any stale entry left under a reused child pid goes.
            match cwd.get(&event.parent_pid).cloned() {
                Some(parent_cwd) if event.parent_pid != event.pid => {
                    cwd.insert(event.pid, parent_cwd);
                }
                _ => {
                    cwd.remove(&event.pid);
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    fn event(kind: u16, pid: u64, parent_pid: u64) -> LifecycleEventWire {
        LifecycleEventWire {
            event: kind,
            pid,
            parent_pid,
            ..LifecycleEventWire::default()
        }
    }

    #[test]
    fn fork_inherits_the_parent_cwd_and_later_chdir_is_independent() {
        let mut cwd = BTreeMap::new();
        cwd.insert(10, "/usr/lib".to_string());
        apply_lifecycle_event(&mut cwd, &event(LIFECYCLE_EVENT_FORK, 11, 10));
        assert_eq!(cwd.get(&11).map(String::as_str), Some("/usr/lib"));
        cwd.insert(10, "/tmp".to_string());
        assert_eq!(cwd.get(&11).map(String::as_str), Some("/usr/lib"));
    }

    #[test]
    fn exit_prunes_and_a_reused_pid_starts_clean() {
        let mut cwd = BTreeMap::new();
        cwd.insert(11, "/old".to_string());
        apply_lifecycle_event(&mut cwd, &event(LIFECYCLE_EVENT_EXIT, 11, 10));
        assert!(!cwd.contains_key(&11));

        // Reuse by a fork from a parent still at the default root.
        cwd.insert(12, "/stale".to_string());
        apply_lifecycle_event(&mut cwd, &event(LIFECYCLE_EVENT_FORK, 12, 10));
        assert!(!cwd.contains_key(&12));
    }

    #[test]
    fn ordered_exit_then_fork_of_a_reused_pid_keeps_only_the_new_process_state() {
        let mut cwd = BTreeMap::new();
        cwd.insert(5, "/dead".to_string());
        cwd.insert(6, "/parent".to_string());
        for e in [
            event(LIFECYCLE_EVENT_EXIT, 5, 6),
            event(LIFECYCLE_EVENT_FORK, 5, 6),
        ] {
            apply_lifecycle_event(&mut cwd, &e);
        }
        assert_eq!(cwd.get(&5).map(String::as_str), Some("/parent"));
    }

    #[test]
    fn malformed_or_unrelated_events_change_nothing() {
        let mut cwd = BTreeMap::new();
        cwd.insert(1, "/a".to_string());
        apply_lifecycle_event(&mut cwd, &event(LIFECYCLE_EVENT_EXIT, 0, 0));
        apply_lifecycle_event(
            &mut cwd,
            &event(rustos_user_abi::syscall::LIFECYCLE_EVENT_EXEC, 1, 0),
        );
        assert_eq!(cwd.get(&1).map(String::as_str), Some("/a"));
        // A self-parented fork is malformed: it must not keep a stale entry.
        apply_lifecycle_event(&mut cwd, &event(LIFECYCLE_EVENT_FORK, 1, 1));
        assert!(!cwd.contains_key(&1));
    }
}
