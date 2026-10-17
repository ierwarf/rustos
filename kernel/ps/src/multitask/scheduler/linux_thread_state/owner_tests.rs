use super::*;

#[test]
fn static_thread_binding_rejects_recycled_owner_before_any_callback() {
    let _table = crate::multitask::process_table::tests::isolate_process_table();
    let _serial = super::super::scheduling_runtime::test_serial_guard();
    let slot = 94;
    let handle = super::super::tests::test_process(0xb301);
    let binding = CurrentLinuxThreadBinding {
        process_handle: handle,
        tid: 0xb301,
        abi: UserAbi::Linux,
        slot,
        linux_thread_state: NonNull::from(&LINUX_THREAD_STATES[slot]),
    };
    {
        let mut cell = LINUX_THREAD_STATES[slot].lock();
        cell.owner_tid = Some(0xb301);
        cell.state = Some(LinuxThreadState::default());
    }
    assert!(
        binding
            .with_thread_state_mut(|state| state.as_mut().unwrap().fs_base = 77)
            .is_some()
    );
    {
        let mut cell = LINUX_THREAD_STATES[slot].lock();
        assert_eq!(cell.state.unwrap().fs_base, 77);
        cell.owner_tid = Some(0xb302);
        cell.state = Some(LinuxThreadState::default());
    }
    assert!(
        binding
            .with_thread_state_mut(|_| panic!("foreign callback invoked"))
            .is_none()
    );
    let mut cell = LINUX_THREAD_STATES[slot].lock();
    assert_eq!(cell.state.unwrap().fs_base, 0);
    cell.owner_tid = None;
    cell.state = None;
}

#[test]
fn pending_hint_tracks_bounded_mutation_under_the_same_thread_leaf() {
    let _table = crate::multitask::process_table::tests::isolate_process_table();
    let _serial = super::super::scheduling_runtime::test_serial_guard();
    let slot = 94;
    let handle = super::super::tests::test_process(0xb302);
    let binding = CurrentLinuxThreadBinding {
        process_handle: handle,
        tid: 0xb302,
        abi: UserAbi::Linux,
        slot,
        linux_thread_state: NonNull::from(&LINUX_THREAD_STATES[slot]),
    };
    {
        let mut cell = LINUX_THREAD_STATES[slot].lock();
        cell.owner_tid = Some(binding.tid);
        cell.state = Some(LinuxThreadState::default());
    }
    binding
        .with_thread_state_mut(|state| state.as_mut().unwrap().pending_signals = 1)
        .unwrap();
    assert!(super::super::super::current_identity::signal_pending(slot));
    binding
        .with_thread_state_mut(|state| state.as_mut().unwrap().pending_signals = 0)
        .unwrap();
    assert!(!super::super::super::current_identity::signal_pending(slot));
    binding
        .with_thread_state_mut(|state| state.as_mut().unwrap().pending_sigchld_events = 1)
        .unwrap();
    assert!(super::super::super::current_identity::signal_pending(slot));
    binding
        .with_thread_state_mut(|state| *state = None)
        .unwrap();
    assert!(!super::super::super::current_identity::signal_pending(slot));
    LINUX_THREAD_STATES[slot].lock().owner_tid = None;
}
