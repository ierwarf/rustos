//! Immutable rootd bootstrap manifest preflight.
//!
//! Rootd owns exactly one service startup graph. The validation below runs
//! once before its endpoint becomes visible. It performs no service IPC,
//! no Job creation and no work on a syscall or scheduler hot path.
//!
//! Future declarative component routing must retain the existing authenticated
//! lease/Job owner until a dual-run cutover is proven. A path or a moniker
//! never grants authority.

use super::service_manifest::INITD_LEASE_ID;
use super::service_manifest::{
    BootstrapServiceSpec, PostInitServiceSpec, BOOTSTRAP_MANIFEST, DEP_LOADERD, DEP_PAGERD,
    DEP_PROCD, DEP_SYSCALLD, DEP_VFSD, POST_INIT_MANIFEST,
};
use rustos_component_model::Moniker;
use rustos_user_abi::syscall::{
    IPC_SERVICE_DEVMGRD, IPC_SERVICE_INPUTD, IPC_SERVICE_NETD, IPC_SERVICE_SESSIOND,
    IPC_SERVICE_STORAGED, IPC_SERVICE_UISERVER,
};
use rustos_user_abi::syscall::{
    IPC_SERVICE_LINUX_SYSCALLD, IPC_SERVICE_LOADERD, IPC_SERVICE_PAGERD, IPC_SERVICE_PROCD,
    IPC_SERVICE_VFSD,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ManifestError {
    EmptyPath,
    MissingTerminator,
    InvalidPath,
    InvalidName,
    DuplicateServiceId,
    DuplicateExecutable,
    UnknownDependency,
    DependencyCycle,
    InvalidScheduling,
}

const ALL_DEPS: u16 = DEP_SYSCALLD | DEP_VFSD | DEP_LOADERD | DEP_PROCD | DEP_PAGERD;

fn checked_exec(path: &[u8]) -> Result<Moniker, ManifestError> {
    if path.last().copied() != Some(0) {
        return Err(ManifestError::MissingTerminator);
    }
    let body = &path[..path.len() - 1];
    if body.is_empty() {
        return Err(ManifestError::EmptyPath);
    }
    let text = core::str::from_utf8(body).map_err(|_| ManifestError::InvalidPath)?;
    if !text.starts_with("services/")
        || text.contains("..")
        || text.contains("//")
        || text.as_bytes().contains(&0)
        || !text.ends_with(".elf")
    {
        return Err(ManifestError::InvalidPath);
    }
    let mut parts = text.split('/');
    if parts.next() != Some("services") {
        return Err(ManifestError::InvalidPath);
    }
    let service = parts.next().ok_or(ManifestError::InvalidPath)?;
    let filename = parts.next().ok_or(ManifestError::InvalidPath)?;
    if parts.next().is_some() || filename.strip_suffix(".elf") != Some(service) {
        return Err(ManifestError::InvalidPath);
    }
    Moniker::new(service).map_err(|_| ManifestError::InvalidName)
}

fn dependency_target(bit: u16) -> Option<u64> {
    match bit {
        DEP_SYSCALLD => Some(IPC_SERVICE_LINUX_SYSCALLD),
        DEP_VFSD => Some(IPC_SERVICE_VFSD),
        DEP_LOADERD => Some(IPC_SERVICE_LOADERD),
        DEP_PROCD => Some(IPC_SERVICE_PROCD),
        DEP_PAGERD => Some(IPC_SERVICE_PAGERD),
        _ => None,
    }
}

fn validate(
    boot: &[BootstrapServiceSpec],
    post: &[PostInitServiceSpec],
) -> Result<(), ManifestError> {
    if boot.is_empty() {
        return Err(ManifestError::EmptyPath);
    }
    for (index, spec) in boot.iter().enumerate() {
        if boot[..index]
            .iter()
            .any(|s| s.service_id == spec.service_id)
            || post.iter().any(|s| s.service_id == spec.service_id)
        {
            return Err(ManifestError::DuplicateServiceId);
        }
        let name = checked_exec(spec.exec_path)?;
        if !spec.scheduling.is_canonical() || spec.scheduling.domain != spec.service_id {
            return Err(ManifestError::InvalidScheduling);
        }
        for earlier in &boot[..index] {
            if checked_exec(earlier.exec_path)? == name {
                return Err(ManifestError::DuplicateExecutable);
            }
        }
        if spec.dependency_mask & !ALL_DEPS != 0 {
            return Err(ManifestError::UnknownDependency);
        }
        for bit in [DEP_SYSCALLD, DEP_VFSD, DEP_LOADERD, DEP_PROCD, DEP_PAGERD] {
            if spec.dependency_mask & bit == 0 {
                continue;
            }
            let target = dependency_target(bit).ok_or(ManifestError::UnknownDependency)?;
            if !boot[..index].iter().any(|s| s.service_id == target) {
                return Err(ManifestError::DependencyCycle);
            }
        }
    }
    for (index, spec) in post.iter().enumerate() {
        if boot.iter().any(|s| s.service_id == spec.service_id)
            || post[..index]
                .iter()
                .any(|s| s.service_id == spec.service_id)
        {
            return Err(ManifestError::DuplicateServiceId);
        }
        let name = checked_exec(spec.exec_path)?;
        if !spec.scheduling.is_canonical() || spec.scheduling.domain != spec.service_id {
            return Err(ManifestError::InvalidScheduling);
        }
        for earlier in boot {
            if checked_exec(earlier.exec_path)? == name {
                return Err(ManifestError::DuplicateExecutable);
            }
        }
        for earlier in &post[..index] {
            if checked_exec(earlier.exec_path)? == name {
                return Err(ManifestError::DuplicateExecutable);
            }
        }
    }
    Ok(())
}

pub(super) fn preflight() -> Result<(), ManifestError> {
    validate(&BOOTSTRAP_MANIFEST, &POST_INIT_MANIFEST)
}

/// Existing service lookup policy, now colocated with manifest validation.
/// This remains a pure, allocation-free predicate; rootd's authenticated
/// sender/lease checks still execute before reaching it.
pub(super) fn route_allowed(subject: u64, target: u64) -> bool {
    if bootstrap_route_allowed(subject, target) {
        return true;
    }
    match subject {
        INITD_LEASE_ID => matches!(
            target,
            IPC_SERVICE_NETD
                | IPC_SERVICE_DEVMGRD
                | IPC_SERVICE_INPUTD
                | IPC_SERVICE_STORAGED
                | IPC_SERVICE_SESSIOND
        ),
        IPC_SERVICE_VFSD => matches!(target, IPC_SERVICE_DEVMGRD | IPC_SERVICE_STORAGED),
        IPC_SERVICE_PROCD => target == IPC_SERVICE_LOADERD,
        IPC_SERVICE_INPUTD => target == IPC_SERVICE_NETD,
        IPC_SERVICE_SESSIOND => matches!(
            target,
            IPC_SERVICE_LOADERD | IPC_SERVICE_DEVMGRD | IPC_SERVICE_UISERVER
        ),
        IPC_SERVICE_UISERVER => target == IPC_SERVICE_INPUTD,
        IPC_SERVICE_DEVMGRD => {
            matches!(target, IPC_SERVICE_SESSIOND | IPC_SERVICE_UISERVER)
        }
        _ => false,
    }
}

pub(super) fn bootstrap_route_allowed(subject: u64, target: u64) -> bool {
    let bit = match target {
        IPC_SERVICE_LINUX_SYSCALLD => DEP_SYSCALLD,
        IPC_SERVICE_VFSD => DEP_VFSD,
        IPC_SERVICE_LOADERD => DEP_LOADERD,
        IPC_SERVICE_PROCD => DEP_PROCD,
        IPC_SERVICE_PAGERD => DEP_PAGERD,
        _ => 0,
    };
    bit != 0
        && BOOTSTRAP_MANIFEST
            .iter()
            .find(|spec| spec.service_id == subject)
            .is_some_and(|spec| spec.dependency_mask & bit != 0)
}

/// A live service principal is exactly one PID, and a post-init service
/// cannot use its reporter's PID as its own. This predicate is evaluated only
/// on the authenticated deferred-spawn registration path; it does not insert
/// any IPC into service calls or syscall hot paths.
///
/// An exited lease is not live authority: a recycled PID can be admitted
/// after that process's old identity has been retired by rootd. A request for
/// the *same* service and live PID is handled separately as an idempotent
/// replay by the rootd lease owner, without changing the reporter.
pub(super) fn admit_unique_live_service_pid(
    core: &[super::Lease],
    post_init: &[super::PostInitLease],
    service_id: u64,
    candidate_pid: u64,
    reporter_pid: u64,
) -> bool {
    if candidate_pid == 0 || candidate_pid == reporter_pid {
        return false;
    }
    if core.iter().any(|lease| {
        lease.pid == candidate_pid && lease.state == super::ROOTD_LEASE_STATE_RUNNING
    }) {
        return false;
    }
    !post_init.iter().any(|lease| {
        lease.service_id != service_id
            && lease.pid == candidate_pid
            && lease.state == super::ROOTD_LEASE_STATE_RUNNING
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_manifest_is_acyclic_and_bounded() {
        assert_eq!(preflight(), Ok(()));
    }

    #[test]
    fn duplicate_service_and_dependency_cycles_fail_closed() {
        let mut repeated = BOOTSTRAP_MANIFEST;
        repeated[1].service_id = repeated[0].service_id;
        assert_eq!(
            validate(&repeated, &POST_INIT_MANIFEST),
            Err(ManifestError::DuplicateServiceId)
        );
        let mut cyclic = BOOTSTRAP_MANIFEST;
        cyclic[0].dependency_mask = DEP_VFSD;
        assert_eq!(
            validate(&cyclic, &POST_INIT_MANIFEST),
            Err(ManifestError::DependencyCycle)
        );
    }

    #[test]
    fn path_validation_does_not_accept_authority_from_a_path() {
        assert!(checked_exec(b"services/../../foo\0").is_err());
        assert!(checked_exec(b"services/a/a.elf").is_err());
        assert_eq!(checked_exec(b"services/a/a.elf\0").unwrap().as_str(), "a");
    }
}
