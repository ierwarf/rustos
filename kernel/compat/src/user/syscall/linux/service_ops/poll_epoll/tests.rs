use super::{
    EpollCandidate, EpollScanMode, EpollTargetGuard, PollReadinessState, epoll_provider_events,
    epoll_report_events, select_epoll_events, acknowledge_epoll_wait_block, acquire_epoll_target_guard,
    console_ready_events, epoll_ctl_requires_live_provider_epoch, linux_abi, multitask,
    note_poll_observation, provider_revoke_events, require_completed_provider_scan,
    sanitize_wait_signal_mask, waitset_provider_query_timeout_ms_from_ticks,
};
use alloc::vec::Vec;
use rustos_user_abi::linux::{SIGKILL, SIGSTOP};
use rustos_user_abi::syscall::{WAITSET_PROVIDER_INPUTD, WAITSET_PROVIDER_NETD, WaitSetClaimWire};

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

fn candidate(events: u32, data: u64, gated: bool) -> EpollCandidate {
    EpollCandidate {
        events,
        data,
        claim: WaitSetClaimWire::default(),
        gated,
    }
}

#[test]
fn epoll_report_masks_to_the_registered_events_and_always_keeps_err_hup() {
    let requested = linux_abi::EPOLLIN | linux_abi::EPOLLET | linux_abi::EPOLLONESHOT;
    let provider = epoll_provider_events(requested);
    assert_eq!(provider, linux_abi::EPOLLIN);
    // OUT was not requested, ERR/HUP always are, mode bits never leak out.
    assert_eq!(
        epoll_report_events(
            linux_abi::EPOLLIN | linux_abi::EPOLLOUT | linux_abi::EPOLLERR,
            requested
        ),
        linux_abi::EPOLLIN | linux_abi::EPOLLERR
    );
    assert_eq!(
        epoll_report_events(linux_abi::EPOLLHUP, linux_abi::EPOLLOUT),
        linux_abi::EPOLLHUP
    );
}

#[test]
fn epoll_rdhup_and_normal_data_aliases_follow_the_registered_mask() {
    let requested = linux_abi::EPOLLRDHUP | linux_abi::EPOLLRDNORM;
    assert_eq!(
        epoll_provider_events(requested),
        linux_abi::EPOLLRDHUP | linux_abi::EPOLLIN
    );
    assert_eq!(
        epoll_report_events(linux_abi::EPOLLIN | linux_abi::EPOLLRDHUP, requested),
        linux_abi::EPOLLRDNORM | linux_abi::EPOLLRDHUP
    );
    // RDHUP is delivered only when it was registered.
    assert_eq!(
        epoll_report_events(linux_abi::EPOLLRDHUP, linux_abi::EPOLLIN),
        0
    );
}

#[test]
fn level_triggered_selection_needs_no_claim_and_honours_maxevents() {
    let candidates = [
        candidate(linux_abi::EPOLLIN, 1, false),
        candidate(linux_abi::EPOLLOUT, 2, false),
        candidate(linux_abi::EPOLLIN, 3, false),
    ];
    let ready = select_epoll_events(7, &candidates, 2, EpollScanMode::Consume).unwrap();
    assert_eq!(
        ready,
        alloc::vec![(linux_abi::EPOLLIN, 1), (linux_abi::EPOLLOUT, 2)]
    );
}

#[test]
fn peeking_never_claims_edge_or_oneshot_reports() {
    // A nested poll must answer "is anything reportable" without consuming it;
    // a claim would require a vfsd exchange, which this test has none of.
    let candidates = [candidate(linux_abi::EPOLLIN, 9, true)];
    let ready = select_epoll_events(7, &candidates, 1, EpollScanMode::Peek).unwrap();
    assert_eq!(ready, alloc::vec![(linux_abi::EPOLLIN, 9)]);
}

#[test]
fn epoll_exclusive_wire_constants_match_linux() {
    assert_eq!(linux_abi::EPOLLET, 0x8000_0000);
    assert_eq!(linux_abi::EPOLLONESHOT, 0x4000_0000);
    assert_eq!(linux_abi::EPOLLWAKEUP, 0x2000_0000);
    assert_eq!(linux_abi::EPOLLEXCLUSIVE, 0x1000_0000);
    assert_eq!(linux_abi::EPOLLRDHUP, 0x2000);
}
