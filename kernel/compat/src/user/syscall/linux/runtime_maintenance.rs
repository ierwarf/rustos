//! Bounded maintenance hooks exported by the Linux syscall router.

pub(crate) fn drain_ipc_call_profile() -> usize {
    super::ipc_profile::drain_ipc_call_profile()
}

pub(crate) fn service_deferred_transfer_releases() -> usize {
    super::service_ops::service_deferred_handle_maintenance()
        .saturating_add(super::ipc_ops::service_deferred_transfer_releases())
}
