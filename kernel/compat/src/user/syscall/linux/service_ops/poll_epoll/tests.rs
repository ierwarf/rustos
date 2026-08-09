use super::{
    EpollTargetGuard, PollReadinessState, acknowledge_epoll_wait_block, acquire_epoll_target_guard,
    console_ready_events, epoll_ctl_requires_live_provider_epoch, linux_abi, multitask,
    note_poll_observation, provider_revoke_events, require_completed_provider_scan,
    sanitize_wait_signal_mask, waitset_provider_query_timeout_ms_from_ticks,
};
use alloc::vec::Vec;
use rustos_user_abi::linux::{SIGKILL, SIGSTOP};
use rustos_user_abi::syscall::{WAITSET_PROVIDER_INPUTD, WAITSET_PROVIDER_NETD};

#[test]
fn console_output_is_writable_only_while_its_session_is_live() {
    assert_eq!(
        console_ready_events(
            multitask::ConsoleStreamKind::Output,
            true,
            false,
            linux_abi::POLLOUT as u32,
        ),
        linux_abi::POLLOUT as u32
    );
    assert_eq!(
        console_ready_events(
            multitask::ConsoleStreamKind::Error,
            false,
            false,
            linux_abi::POLLHUP as u32,
        ),
        linux_abi::POLLHUP as u32
    );
    assert_eq!(
        console_ready_events(
            multitask::ConsoleStreamKind::Input,
            true,
            false,
            linux_abi::POLLIN as u32,
        ),
        0
    );
}

#[test]
fn object_observations_are_deduplicated_and_keep_the_newest_generation() {
    let mut state = PollReadinessState {
        ready: 0,
        provider_timed_out: false,
        observations: Vec::new(),
    };
    note_poll_observation(&mut state, WAITSET_PROVIDER_NETD, 41, 7);
    note_poll_observation(&mut state, WAITSET_PROVIDER_INPUTD, 1, 11);
    note_poll_observation(&mut state, WAITSET_PROVIDER_NETD, 41, 7);
    note_poll_observation(&mut state, WAITSET_PROVIDER_NETD, 42, 7);
    assert_eq!(state.observations.len(), 3);
    assert_eq!(state.observations[0].provider, WAITSET_PROVIDER_NETD);
    assert_eq!(state.observations[0].object_id, 41);
    assert_eq!(state.observations[1].object_id, 42);
    assert_eq!(state.observations[2].provider, WAITSET_PROVIDER_INPUTD);
    note_poll_observation(&mut state, WAITSET_PROVIDER_NETD, 41, 8);
    assert_eq!(state.observations[0].generation, 8);
}

#[test]
fn temporary_wait_mask_cannot_block_kill_or_stop() {
    let kill = 1_u64 << (SIGKILL - 1);
    let stop = 1_u64 << (SIGSTOP - 1);
    let ordinary = 1_u64 << (2 - 1);
    assert_eq!(sanitize_wait_signal_mask(kill | stop | ordinary), ordinary);
}

#[test]
fn provider_query_timeout_never_exceeds_the_wait_deadline_or_service_cap() {
    assert_eq!(
        waitset_provider_query_timeout_ms_from_ticks(10, 10, 1000),
        1
    );
    assert_eq!(
        waitset_provider_query_timeout_ms_from_ticks(10, 15, 1000),
        5
    );
    assert_eq!(
        waitset_provider_query_timeout_ms_from_ticks(10, 100, 1000),
        16
    );
    assert_eq!(
        waitset_provider_query_timeout_ms_from_ticks(10, 11, 100),
        10
    );
}

#[test]
fn provider_timeout_never_hides_readiness_found_earlier_in_the_scan() {
    assert_eq!(require_completed_provider_scan(true, true), Ok(()));
    assert_eq!(require_completed_provider_scan(false, false), Ok(()));
    assert_eq!(
        require_completed_provider_scan(true, false),
        Err(super::LINUX_ETIMEDOUT)
    );
}

#[test]
fn provider_revoke_is_reported_per_fd_as_error_and_hup() {
    let expected = linux_abi::POLLERR as u32 | linux_abi::POLLHUP as u32;
    for errno in [
        super::LINUX_EBADF,
        super::LINUX_ENODEV,
        super::LINUX_EPIPE,
        super::LINUX_ENOSYS,
    ] {
        assert_eq!(provider_revoke_events(errno), Some(expected));
    }
    assert_eq!(provider_revoke_events(super::LINUX_EIO), None);
    assert_eq!(provider_revoke_events(super::LINUX_ETIMEDOUT), None);
}

#[test]
fn transient_vfs_reply_break_is_retried_inside_epoll_wait() {
    assert!(super::transient_waitset_scan_error(super::LINUX_EPIPE));
    assert!(!super::transient_waitset_scan_error(super::LINUX_EBADF));
    assert!(!super::transient_waitset_scan_error(super::LINUX_ENODEV));
}

#[test]
fn epoll_delete_does_not_require_a_live_provider_epoch() {
    assert!(epoll_ctl_requires_live_provider_epoch(
        linux_abi::EPOLL_CTL_ADD
    ));
    assert!(epoll_ctl_requires_live_provider_epoch(
        linux_abi::EPOLL_CTL_MOD
    ));
    assert!(!epoll_ctl_requires_live_provider_epoch(
        linux_abi::EPOLL_CTL_DEL
    ));
}

#[test]
fn epoll_ctl_guard_pins_console_across_concurrent_final_close() {
    let console = multitask::ConsoleHandle::new(multitask::ConsoleStreamKind::Input);
    let token = console.token_id();
    let guard = acquire_epoll_target_guard(&multitask::KernelHandle::Console(console.clone()))
        .expect("live console target");

    assert!(!console.release_descriptor_reference());
    assert!(multitask::ConsoleHandle::token_is_live(token));
    let EpollTargetGuard::Console(pinned) = guard else {
        panic!("console target must use a console guard");
    };
    assert!(pinned.release_descriptor_reference());
    assert!(!multitask::ConsoleHandle::token_is_live(token));
}

#[test]
fn epoll_wait_reuse_disarms_previous_deadline_after_wake() {
    struct DeadlineCleanup {
        task_id: u64,
        disarm: fn(u64) -> bool,
    }

    impl Drop for DeadlineCleanup {
        fn drop(&mut self) {
            (self.disarm)(self.task_id);
        }
    }

    let task_id = 0xfeed_u64;
    let first_deadline = 100_u64;
    let second_deadline = 200_u64;
    let disarm: fn(u64) -> bool = crate::arch::rtc::disarm_sleep_waiter;
    let _cleanup = DeadlineCleanup { task_id, disarm };
    assert!(crate::arch::rtc::arm_sleep_waiter_until_tick(
        task_id,
        first_deadline
    ));

    // A provider wake and successful scheduler commit acknowledge the
    // first arm before this task installs its longer replacement wait.
    acknowledge_epoll_wait_block(task_id);
    assert!(!(disarm)(task_id), "post-commit timer owner leaked");

    assert!(crate::arch::rtc::arm_sleep_waiter_until_tick(
        task_id,
        second_deadline
    ));
    assert!((disarm)(task_id), "replacement deadline was not admitted");
}
