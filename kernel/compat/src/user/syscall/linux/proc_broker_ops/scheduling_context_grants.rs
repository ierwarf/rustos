//! Rootd-authored, kernel-sealed scheduling-context launch grants.
//!
//! This bounded registry owns the one-shot token from rootd publication until
//! exact loader commit or requester/rootd retirement. A token never survives
//! epoch, process-generation, executable-path, or policy substitution.

use super::*;
use rustos_user_abi::syscall::{
    RustosSchedulingContextAuthority, RustosSchedulingContextGrantBrokerArgs,
    RustosSchedulingContextPolicy, SCHEDULING_CONTEXT_POLICY_ABI_VERSION,
};

const MAX_SCHEDULING_CONTEXT_GRANTS: usize = 128;

static NEXT_SCHEDULING_CONTEXT_GRANT: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Eq, PartialEq)]
struct SchedulingContextGrant {
    rootd: multitask::ProcessIdentity,
    rootd_epoch: u64,
    requester: multitask::ProcessIdentity,
    exec_path: String,
    policy: RustosSchedulingContextPolicy,
}

type SchedulingContextGrantRegistry =
    FnvIndexMap<u64, SchedulingContextGrant, MAX_SCHEDULING_CONTEXT_GRANTS>;

static SCHEDULING_CONTEXT_GRANTS: TrackedSpinLock<
    SchedulingContextGrantRegistry,
    { LockClass::ProcBrokerRegistry as u8 },
> = TrackedSpinLock::new(FnvIndexMap::new());

pub(super) fn grant(args_ptr: u64) -> u64 {
    let Some(rootd) = multitask::current_user_process_identity() else {
        return linux_errno(LINUX_EPERM);
    };
    let Some((rootd_owner, rootd_epoch)) =
        ipc_ops::live_service_endpoint_owner_and_epoch(IPC_SERVICE_ROOTD)
    else {
        return linux_errno(LINUX_EPERM);
    };
    if rootd.process_id() != rootd_owner {
        return linux_errno(LINUX_EPERM);
    }
    let args =
        match usermem::read_current_user_struct::<RustosSchedulingContextGrantBrokerArgs>(args_ptr)
        {
            Ok(args) => args,
            Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
        };
    if args.abi_version != SCHEDULING_CONTEXT_POLICY_ABI_VERSION
        || args.reserved0 != 0
        || args.flags != 0
        || args.requester_pid == 0
        || !args.policy.is_canonical()
    {
        return linux_errno(LINUX_EINVAL);
    }
    let Some(requester) = multitask::live_user_process_identity_by_pid(args.requester_pid) else {
        return linux_errno(LINUX_ESRCH);
    };
    let exec_path = match read_user_text(args.exec_path_ptr, args.exec_path_len) {
        Ok(path) => path,
        Err(errno) => return linux_errno(errno),
    };
    let mut grants = SCHEDULING_CONTEXT_GRANTS.lock();
    if grants.len() >= MAX_SCHEDULING_CONTEXT_GRANTS {
        return linux_errno(LINUX_EAGAIN);
    }
    let Some(token) = allocate_token(&grants) else {
        return linux_errno(LINUX_EOVERFLOW);
    };
    let grant = SchedulingContextGrant {
        rootd,
        rootd_epoch,
        requester,
        exec_path,
        policy: args.policy,
    };
    if grants.insert(token, grant).is_err() {
        return linux_errno(LINUX_EAGAIN);
    }
    token
}

pub(super) fn consume(
    authority: RustosSchedulingContextAuthority,
    requester_pid: u64,
    exec_path: &str,
) -> Result<RustosSchedulingContextPolicy, i64> {
    if authority.token == 0 || !authority.policy.is_canonical() {
        return Err(LINUX_EINVAL);
    }
    let grant = SCHEDULING_CONTEXT_GRANTS
        .lock()
        .remove(&authority.token)
        .ok_or(LINUX_EPERM)?;
    if grant.policy != authority.policy {
        return Err(LINUX_EPERM);
    }
    validate_consumed_grant(grant, requester_pid, exec_path)
}

pub(super) fn consume_direct_bootstrap(
    requester_pid: u64,
    exec_path: &str,
) -> Result<RustosSchedulingContextPolicy, i64> {
    let mut grants = SCHEDULING_CONTEXT_GRANTS.lock();
    let mut matched = grants.iter().filter_map(|(token, grant)| {
        (grant.requester.process_id() == requester_pid && grant.exec_path == exec_path)
            .then_some(*token)
    });
    let token = matched.next().ok_or(LINUX_EPERM)?;
    if matched.next().is_some() {
        return Err(LINUX_EBUSY);
    }
    let grant = grants.remove(&token).ok_or(LINUX_EPERM)?;
    drop(grants);
    validate_consumed_grant(grant, requester_pid, exec_path)
}

fn validate_consumed_grant(
    grant: SchedulingContextGrant,
    requester_pid: u64,
    exec_path: &str,
) -> Result<RustosSchedulingContextPolicy, i64> {
    let live_requester = multitask::live_user_process_identity_by_pid(requester_pid);
    let live_rootd = ipc_ops::live_service_endpoint_owner_and_epoch(IPC_SERVICE_ROOTD);
    if live_requester != Some(grant.requester)
        || grant.requester.process_id() != requester_pid
        || live_rootd != Some((grant.rootd.process_id(), grant.rootd_epoch))
        || multitask::live_user_process_identity_by_pid(grant.rootd.process_id())
            != Some(grant.rootd)
        || grant.exec_path != exec_path
    {
        return Err(LINUX_EPERM);
    }
    Ok(grant.policy)
}

pub(super) fn revoke_for_process(process_id: u64) {
    SCHEDULING_CONTEXT_GRANTS.lock().retain(|_, grant| {
        grant.rootd.process_id() != process_id && grant.requester.process_id() != process_id
    });
}

fn allocate_token(grants: &SchedulingContextGrantRegistry) -> Option<u64> {
    for _ in 0..MAX_SCHEDULING_CONTEXT_GRANTS {
        let token = allocate_nonwrapping_broker_identity(&NEXT_SCHEDULING_CONTEXT_GRANT)?;
        if !grants.contains_key(&token) {
            return Some(token);
        }
    }
    None
}
