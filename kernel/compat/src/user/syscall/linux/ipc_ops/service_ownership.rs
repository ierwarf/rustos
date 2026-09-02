//! Service-endpoint ownership, capability, and lookup predicates.
//!
//! - **Owner:** this module owns every question of the form "does process P
//!   own service S" and "which capabilities does P hold". The registry arrays
//!   themselves stay with the publication path in `ipc_ops`; nothing here
//!   registers, revokes, or advances an epoch.
//! - **Boundary:** a process id is untrusted. Answers are derived from the
//!   published registry, never from a caller-supplied claim.
//! - **Concurrency:** two classes of predicate live here and they are not
//!   interchangeable. An *authorization* answer needs the exact
//!   endpoint/owner/capability tuple and takes the registry lock. A
//!   *membership* answer reads one atomic and is conservative by construction;
//!   it exists because the anonymous-mmap path is hot enough that taking the
//!   registry lock on every mapping contends with registration and lookup.
//! - **Failure:** every predicate fails closed on an unknown service, a zero
//!   process id, or an exiting owner.
//! - **Forbidden:** no membership predicate may be used for authorization.

use super::*;

/// Process that owns the live pager-policy capability, or `0`.
///
/// The anonymous-mmap broker must know whether it is mapping *for* the pager,
/// and it is hot enough that taking the service-endpoint registry lock on
/// every mapping contends with registration and lookup. The owner changes only
/// when a service endpoint is published, so publish it once here and let the
/// broker read one atomic.
pub(super) static PAGER_POLICY_OWNER: AtomicU64 = AtomicU64::new(0);

/// Whether `process_id` owns the live pager-policy capability. Lock-free.
pub(crate) fn process_owns_pager_policy(process_id: u64) -> bool {
    // ORDERING: Acquire pairs with the registration Release above.
    process_id != 0 && PAGER_POLICY_OWNER.load(Ordering::Acquire) == process_id
}

/// Withdraws the published pager-policy owner once its endpoint is gone.
///
/// Callers store this *after* clearing the endpoint, so the exclusion window
/// stays a superset of the transport's lifetime: a mapping that races the
/// withdrawal is wired rather than demand-backed by a pager that is leaving.
///
/// A compare-exchange rather than an unconditional store, so a late revoke of
/// a previous owner cannot erase a newer one that has already republished.
/// Without this the owner outlived its process and a recycled pid would have
/// inherited the exclusion.
pub(super) fn withdraw_pager_policy_owner(process_id: u64) {
    if process_id == 0 {
        return;
    }
    // ORDERING: Release publishes the withdrawal after the endpoint has
    // already been cleared, keeping the exclusion window a superset of the
    // transport's lifetime. Relaxed on failure: a mismatch means a newer owner
    // already republished and there is nothing to order against.
    let _ =
        PAGER_POLICY_OWNER.compare_exchange(process_id, 0, Ordering::Release, Ordering::Relaxed);
}

pub(super) fn current_process_has_service_capability_locked(
    capability: u64,
    process_id: u64,
) -> bool {
    if capability == 0 || multitask::is_user_process_exiting(process_id) {
        return false;
    }
    SERVICE_ENDPOINTS
        .iter()
        .zip(SERVICE_ENDPOINT_OWNERS.iter())
        .zip(SERVICE_ENDPOINT_CAPS.iter())
        .any(|((endpoint, owner), caps)| {
            // Readers share the mutation critical section so this three-field
            // tuple cannot combine generations during revoke/republication.
            endpoint.load(Ordering::Acquire) != 0
                && owner.load(Ordering::Acquire) == process_id
                && caps.load(Ordering::Acquire) & capability == capability
        })
}

pub(super) fn current_process_can_lookup_service_endpoint() -> bool {
    current_process_has_service_capability(
        rustos_user_abi::syscall::IPC_SERVICE_CAP_ROOT_SUPERVISOR,
    )
}

pub(super) fn service_endpoint_raw(service_id: u64) -> Option<u64> {
    let index = service_index(service_id)?;
    Some(
        stable_published_service_endpoint(index)
            .map(|publication| publication.endpoint)
            .unwrap_or(0),
    )
}

pub(super) fn stable_published_service_endpoint(index: usize) -> Option<PublishedServiceEndpoint> {
    // Publication stores the endpoint last and revocation clears it first.
    // An epoch/endpoint double-read therefore gives the IPC hot path a stable
    // tuple without bouncing the global mutation-lock cache line on every
    // service call. A concurrent mutation is reported as transient absence;
    // callers already treat service restart/revoke as a bounded failure.
    for _ in 0..SERVICE_ENDPOINT_STABLE_READ_ATTEMPTS {
        let epoch_before = SERVICE_ENDPOINT_EPOCHS[index].load(Ordering::Acquire);
        let endpoint_before = SERVICE_ENDPOINTS[index].load(Ordering::Acquire);
        if endpoint_before == 0 {
            return None;
        }
        let owner = SERVICE_ENDPOINT_OWNERS[index].load(Ordering::Acquire);
        let endpoint_after = SERVICE_ENDPOINTS[index].load(Ordering::Acquire);
        let epoch_after = SERVICE_ENDPOINT_EPOCHS[index].load(Ordering::Acquire);
        if endpoint_before == endpoint_after && epoch_before == epoch_after {
            let endpoint = stable_service_endpoint_snapshot(
                endpoint_before,
                owner,
                multitask::is_user_process_exiting(owner),
            );
            return (endpoint != 0 && epoch_before != 0).then_some(PublishedServiceEndpoint {
                endpoint,
                owner,
                epoch: epoch_before,
            });
        }
        core::hint::spin_loop();
    }
    None
}

pub(super) fn stable_service_endpoint_snapshot(
    endpoint: u64,
    owner: u64,
    owner_exiting: bool,
) -> u64 {
    if endpoint == 0 || owner == 0 || owner_exiting {
        0
    } else {
        endpoint
    }
}

pub(super) fn service_index(service_id: u64) -> Option<usize> {
    let index = usize::try_from(service_id).ok()?;
    (index < MAX_SERVICE_ENDPOINTS).then_some(index)
}

pub(crate) fn process_owns_live_service_endpoint(process_id: u64, service_id: u64) -> bool {
    let Some(index) = service_index(service_id) else {
        return false;
    };
    let _registry = SERVICE_ENDPOINT_REGISTRY_MUTATION.lock();
    SERVICE_ENDPOINTS[index].load(Ordering::Acquire) != 0
        && SERVICE_ENDPOINT_OWNERS[index].load(Ordering::Acquire) == process_id
        && !multitask::is_user_process_exiting(process_id)
}

/// Whether `process_id` is the *published* owner of `service_id`, read without
/// the registry lock.
///
/// This answers membership only. It deliberately does not read the endpoint,
/// capability, or exiting state, so it never combines fields from different
/// publication generations - there is only one field.
///
/// The answer is conservative by construction, and the publication order is
/// what makes it so. Registration stores the owner *before* the endpoint
/// (`SERVICE_ENDPOINTS` is published last); revoke clears the endpoint
/// *before* the owner. The interval during which this returns `true` is
/// therefore a strict superset of the interval during which the endpoint is
/// reachable. A caller that must fail closed on membership - the wired
/// pager-control-graph classification - gets `true` for slightly longer than
/// the service exists and never gets `false` while it exists.
///
/// Do not use this for authorization. A capability decision needs the exact
/// endpoint/owner/capability tuple and must take the registry lock.
pub(crate) fn process_owns_published_service_endpoint(process_id: u64, service_id: u64) -> bool {
    if process_id == 0 {
        return false;
    }
    let Some(index) = service_index(service_id) else {
        return false;
    };
    // ORDERING: Acquire pairs with the registration Release that publishes the
    // owner before the endpoint, and with the revoke Release that clears the
    // owner after it.
    SERVICE_ENDPOINT_OWNERS[index].load(Ordering::Acquire) == process_id
}
