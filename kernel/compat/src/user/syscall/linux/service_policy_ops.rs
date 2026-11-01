//! Kernel-held service policy, installed by rootd and enforced without rootd.
//!
//! - **Owner:** rootd authors the policy (which service may look up which, and
//!   which typed capabilities each service role holds) and authenticates which
//!   live process is which service. This module only stores what rootd
//!   installed and answers lookups and registrations from that table.
//! - **Why:** a lookup or a registration used to make the kernel call rootd,
//!   a ring3 service, synchronously from inside the syscall, for a decision
//!   that is a pure function of the static manifests plus "which service is
//!   this pid". Delegating the authority ahead of time (the capability-routing
//!   shape of seL4 delegation and the Fuchsia component framework, where the
//!   authority routes a capability once and the holder just uses it) removes
//!   the upcall, the rootd round trip from the boot path, and the deadlock
//!   shape of a syscall that waits on a service that may be waiting on it.
//! - **Boundary:** only a holder of `RootSupervisor` may install or bind. A
//!   binding names a process id; process ids are allocated from a monotonic
//!   counter that never wraps, so a bound pid can never be a different process.
//! - **Failure:** a caller with no binding, or any lookup before a policy is
//!   installed, is `Unbound`, which the caller treats as "ask the old way" and
//!   counts. A bound caller that the policy does not allow is denied here.
//! - **Forbidden:** no numeric capability mask crosses this ABI; capabilities
//!   are typed discriminants validated against `ServiceCapability`.
use super::*;
use core::mem::size_of;
use nucleus_core::util::lockdep::{LockClass, TrackedSpinLock};
use rustos_user_abi::syscall::{
    RustosServicePolicyArgs, ServiceCapability, ServiceCapabilitySet, SERVICE_POLICY_ABI_VERSION,
    SERVICE_POLICY_MAX_CAPABILITIES, SERVICE_POLICY_MAX_ROUTES, SERVICE_POLICY_MAX_SERVICES,
    SERVICE_POLICY_OP_BIND_PRINCIPAL, SERVICE_POLICY_OP_INSTALL,
};

const SERVICES: usize = SERVICE_POLICY_MAX_SERVICES;
const MAX_PRINCIPALS: usize = 64;
const EACCES: i64 = 13;
const ENOSPC: i64 = 28;
const ESTALE: i64 = 116;

/// What the installed policy says about one lookup.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::user::syscall::linux) enum Decision {
    Allowed,
    Denied,
    /// No policy installed yet, or the caller has no service binding.
    Unbound,
}

#[derive(Clone, Copy)]
pub(super) struct Policy {
    generation: u64,
    routes: [[bool; SERVICES]; SERVICES],
    capabilities: [ServiceCapabilitySet; SERVICES],
    /// `(pid, service)`; a zero pid is an empty slot.
    principals: [(u64, u8); MAX_PRINCIPALS],
}

impl Policy {
    pub(super) const fn new() -> Self {
        Self {
            generation: 0,
            routes: [[false; SERVICES]; SERVICES],
            capabilities: [ServiceCapabilitySet::EMPTY; SERVICES],
            principals: [(0, 0); MAX_PRINCIPALS],
        }
    }

    fn installed(&self) -> bool {
        self.generation != 0
    }

    /// Validates `args` completely before replacing anything.
    pub(super) fn install(&mut self, args: &RustosServicePolicyArgs) -> Result<(), i64> {
        if args.generation <= self.generation
            || usize::from(args.route_count) > SERVICE_POLICY_MAX_ROUTES
            || usize::from(args.capability_count) > SERVICE_POLICY_MAX_SERVICES
        {
            return Err(if args.generation <= self.generation { ESTALE } else { LINUX_EINVAL });
        }
        let mut routes = [[false; SERVICES]; SERVICES];
        for route in &args.routes[..usize::from(args.route_count)] {
            let (subject, target) = (usize::from(route.subject), usize::from(route.target));
            if subject == 0 || target == 0 || subject >= SERVICES || target >= SERVICES
                || subject == target
            {
                return Err(LINUX_EINVAL);
            }
            routes[subject][target] = true;
        }
        let mut capabilities = [ServiceCapabilitySet::EMPTY; SERVICES];
        let mut seen = [false; SERVICES];
        for entry in &args.capabilities[..usize::from(args.capability_count)] {
            let service = usize::from(entry.service);
            if service == 0 || service >= SERVICES || seen[service]
                || usize::from(entry.count) > SERVICE_POLICY_MAX_CAPABILITIES
                || entry.count == 0
            {
                return Err(LINUX_EINVAL);
            }
            seen[service] = true;
            let mut set = ServiceCapabilitySet::EMPTY;
            for id in &entry.capabilities[..usize::from(entry.count)] {
                set = set.with(ServiceCapability::from_id(*id).ok_or(LINUX_EINVAL)?);
            }
            capabilities[service] = set;
        }
        self.routes = routes;
        self.capabilities = capabilities;
        self.generation = args.generation;
        Ok(())
    }

    /// Records that `pid` is `service`. `is_dead` lets a full table reclaim the
    /// slot of a process that has exited.
    pub(super) fn bind(
        &mut self,
        generation: u64,
        pid: u64,
        service: u64,
        is_dead: impl Fn(u64) -> bool,
    ) -> Result<(), i64> {
        let service = u8::try_from(service).map_err(|_| LINUX_EINVAL)?;
        if !self.installed() || generation != self.generation {
            return Err(ESTALE);
        }
        if pid == 0 || service == 0 || usize::from(service) >= SERVICES {
            return Err(LINUX_EINVAL);
        }
        let slot = self
            .principals
            .iter()
            .position(|(bound, _)| *bound == pid)
            .or_else(|| self.principals.iter().position(|(bound, _)| *bound == 0))
            .or_else(|| self.principals.iter().position(|(bound, _)| is_dead(*bound)))
            .ok_or(ENOSPC)?;
        self.principals[slot] = (pid, service);
        Ok(())
    }

    pub(super) fn principal_service(&self, pid: u64) -> Option<usize> {
        if pid == 0 {
            return None;
        }
        self.principals
            .iter()
            .find(|(bound, _)| *bound == pid)
            .map(|(_, service)| usize::from(*service))
    }

    pub(super) fn lookup(&self, pid: u64, target: u64) -> Decision {
        let Some(subject) = self.principal_service(pid).filter(|_| self.installed()) else {
            return Decision::Unbound;
        };
        match usize::try_from(target) {
            Ok(target) if target < SERVICES && self.routes[subject][target] => Decision::Allowed,
            _ => Decision::Denied,
        }
    }

    /// The capabilities `service` may hold when `pid` registers it, or `None`
    /// when this caller is not bound (the old path decides).
    pub(super) fn registration(&self, pid: u64, service: u64) -> Option<Result<ServiceCapabilitySet, i64>> {
        let subject = self.principal_service(pid).filter(|_| self.installed())?;
        let wanted = usize::try_from(service).ok().filter(|index| *index < SERVICES);
        if wanted != Some(subject) {
            return Some(Err(EACCES));
        }
        let set = self.capabilities[subject];
        Some(if set.is_empty() { Err(LINUX_EINVAL) } else { Ok(set) })
    }
}

static POLICY: TrackedSpinLock<Policy, { LockClass::ProcBrokerRegistry as u8 }> =
    TrackedSpinLock::new(Policy::new());

pub(in crate::user::syscall::linux) fn lookup_decision(pid: u64, target: u64) -> Decision {
    POLICY.lock().lookup(pid, target)
}

/// Binds a process the kernel itself just created suspended. Unlike the rootd
/// syscall this needs no generation: the kernel is the creator, and the
/// currently installed policy is by definition the one the child runs under.
pub(in crate::user::syscall::linux) fn bind_spawned(pid: u64, service: u64) -> Result<(), i64> {
    let mut policy = POLICY.lock();
    let generation = policy.generation;
    policy.bind(generation, pid, service, |bound| multitask::is_user_process_exiting(bound))
}

pub(in crate::user::syscall::linux) fn registration_decision(pid: u64, service: u64) -> Option<Result<ServiceCapabilitySet, i64>> {
    POLICY.lock().registration(pid, service)
}

pub(super) fn syscall_linux_rustos_service_policy(ptr: u64) -> u64 {
    if !ipc_ops::current_process_has_service_capability(ServiceCapability::RootSupervisor) {
        return linux_errno(LINUX_EPERM);
    }
    let args = match usermem::read_current_user_struct::<RustosServicePolicyArgs>(ptr) {
        Ok(args) => args,
        Err(error) => return linux_errno(address_space_error_to_linux_errno(error)),
    };
    if args.abi_version != SERVICE_POLICY_ABI_VERSION {
        return linux_errno(LINUX_EINVAL);
    }
    let result = {
        let mut policy = POLICY.lock();
        match args.op {
            SERVICE_POLICY_OP_INSTALL => policy.install(&args),
            SERVICE_POLICY_OP_BIND_PRINCIPAL => policy.bind(
                args.generation,
                args.principal_pid,
                args.principal_service,
                |pid| multitask::is_user_process_exiting(pid),
            ),
            _ => Err(LINUX_EINVAL),
        }
    };
    match result {
        Ok(()) => 0,
        Err(errno) => linux_errno(errno),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustos_user_abi::syscall::{ServiceCapabilityListWire, ServiceRouteWire};

    fn args(generation: u64) -> RustosServicePolicyArgs {
        let mut args = RustosServicePolicyArgs::default();
        args.op = SERVICE_POLICY_OP_INSTALL;
        args.generation = generation;
        args.routes[0] = ServiceRouteWire { subject: 15, target: 6 };
        args.routes[1] = ServiceRouteWire { subject: 15, target: 2 };
        args.route_count = 2;
        let mut list = ServiceCapabilityListWire::default();
        list.service = 6;
        list.count = 1;
        list.capabilities[0] = ServiceCapability::ProcessLoader as u8;
        args.capabilities[0] = list;
        args.capability_count = 1;
        args
    }

    fn alive(_: u64) -> bool {
        false
    }

    #[test]
    fn nothing_is_decided_before_a_policy_is_installed_or_a_caller_is_bound() {
        let mut policy = Policy::new();
        assert_eq!(policy.lookup(40, 6), Decision::Unbound);
        assert!(policy.registration(40, 6).is_none());
        assert_eq!(policy.bind(1, 40, 15, alive), Err(ESTALE), "no policy to bind under");
        policy.install(&args(1)).unwrap();
        assert_eq!(policy.lookup(40, 6), Decision::Unbound, "installed but pid 40 unbound");
    }

    #[test]
    fn a_bound_caller_is_allowed_exactly_the_installed_routes() {
        let mut policy = Policy::new();
        policy.install(&args(1)).unwrap();
        policy.bind(1, 40, 15, alive).unwrap();
        assert_eq!(policy.lookup(40, 6), Decision::Allowed);
        assert_eq!(policy.lookup(40, 2), Decision::Allowed);
        assert_eq!(policy.lookup(40, 3), Decision::Denied, "no offer to netd");
        assert_eq!(policy.lookup(40, 99), Decision::Denied);
    }

    #[test]
    fn registration_requires_the_bound_identity_and_yields_its_typed_capabilities() {
        let mut policy = Policy::new();
        policy.install(&args(1)).unwrap();
        policy.bind(1, 41, 6, alive).unwrap();
        let set = policy.registration(41, 6).unwrap().unwrap();
        assert!(set.contains(ServiceCapability::ProcessLoader));
        assert!(!set.contains(ServiceCapability::RootSupervisor));
        assert_eq!(policy.registration(41, 2), Some(Err(EACCES)), "bound as loaderd, not vfsd");
        policy.bind(1, 42, 2, alive).unwrap();
        assert_eq!(policy.registration(42, 2), Some(Err(LINUX_EINVAL)), "a role with no capabilities");
    }

    #[test]
    fn a_policy_is_validated_whole_before_it_replaces_the_old_one() {
        let mut policy = Policy::new();
        policy.install(&args(2)).unwrap();
        assert_eq!(policy.install(&args(2)), Err(ESTALE), "generation must increase");
        assert_eq!(policy.install(&args(1)), Err(ESTALE));
        for mutate in [
            |a: &mut RustosServicePolicyArgs| a.routes[0].subject = 0,
            |a: &mut RustosServicePolicyArgs| a.routes[0].target = 16,
            |a: &mut RustosServicePolicyArgs| a.routes[0].target = 15,
            |a: &mut RustosServicePolicyArgs| a.capabilities[0].capabilities[0] = 4,
            |a: &mut RustosServicePolicyArgs| a.capabilities[0].count = 0,
            |a: &mut RustosServicePolicyArgs| a.capabilities[0].count = 15,
            |a: &mut RustosServicePolicyArgs| a.route_count = 65,
        ] {
            let mut bad = args(3);
            mutate(&mut bad);
            assert_eq!(policy.install(&bad), Err(LINUX_EINVAL));
        }
        let mut duplicate = args(3);
        duplicate.capabilities[1] = duplicate.capabilities[0];
        duplicate.capability_count = 2;
        assert_eq!(policy.install(&duplicate), Err(LINUX_EINVAL));
        policy.bind(2, 40, 15, alive).unwrap();
        assert_eq!(policy.lookup(40, 6), Decision::Allowed, "failed installs changed nothing");
    }

    #[test]
    fn rebinding_replaces_and_a_full_table_reclaims_only_dead_processes() {
        let mut policy = Policy::new();
        policy.install(&args(1)).unwrap();
        policy.bind(1, 40, 15, alive).unwrap();
        policy.bind(1, 40, 6, alive).unwrap();
        assert_eq!(policy.principal_service(40), Some(6));
        for pid in 100..100 + (MAX_PRINCIPALS as u64 - 1) {
            policy.bind(1, pid, 6, alive).unwrap();
        }
        assert_eq!(policy.bind(1, 999, 6, |_| false), Err(ENOSPC));
        policy.bind(1, 999, 6, |pid| pid == 105).unwrap();
        assert_eq!(policy.principal_service(105), None);
        assert_eq!(policy.principal_service(999), Some(6));
        assert_eq!(policy.bind(7, 5, 6, alive), Err(ESTALE), "stale generation");
    }

    #[test]
    fn the_abi_struct_is_small_and_carries_no_mask_field() {
        assert!(size_of::<RustosServicePolicyArgs>() < 1024);
    }
}
