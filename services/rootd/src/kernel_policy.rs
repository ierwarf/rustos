//! Rootd's delegation of service routing authority to the kernel.
//!
//! The kernel used to call rootd, synchronously and from inside a syscall, to
//! ask two questions whose answers are pure functions of the static manifests
//! plus "which service is this process": may service A look up service B, and
//! which capabilities may the registrant of service S hold. Rootd now answers
//! both once, ahead of time: `install` hands the kernel the typed route table
//! and per-service capability lists, and `bind` tells it which live process is
//! which service at the moment a lease becomes live (before the child is
//! activated, so it can never ask first). The kernel then decides from its own
//! table. Nothing here is a mask: capabilities cross as typed discriminants.
use core::sync::atomic::{AtomicU64, Ordering};
use rustos_user_abi::syscall::{
    RustosServicePolicyArgs, ServiceCapabilityListWire, ServiceRouteWire,
    SERVICE_POLICY_MAX_ROUTES,
    SERVICE_POLICY_MAX_SERVICES, SERVICE_POLICY_OP_BIND_PRINCIPAL, SERVICE_POLICY_OP_INSTALL,
    SYS_RUSTOS_SERVICE_POLICY,
};

use super::component_policy;

/// The policy generation the kernel holds; zero until `install` succeeds.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Builds the route table and capability lists from the static manifests.
pub(super) fn build_policy(generation: u64) -> Result<RustosServicePolicyArgs, i32> {
    let mut args = RustosServicePolicyArgs {
        op: SERVICE_POLICY_OP_INSTALL,
        generation,
        ..RustosServicePolicyArgs::default()
    };
    let mut routes = 0_usize;
    for subject in 1..SERVICE_POLICY_MAX_SERVICES as u64 {
        for target in 1..SERVICE_POLICY_MAX_SERVICES as u64 {
            if subject != target && component_policy::route_allowed(subject, target) {
                if routes == SERVICE_POLICY_MAX_ROUTES {
                    return Err(28);
                }
                args.routes[routes] = ServiceRouteWire {
                    subject: subject as u8,
                    target: target as u8,
                };
                routes += 1;
            }
        }
    }
    args.route_count = routes as u16;
    let mut lists = 0_usize;
    for service in 1..SERVICE_POLICY_MAX_SERVICES as u64 {
        let Some(capability) = component_policy::service_policy_capability(service) else {
            continue;
        };
        let mut list = ServiceCapabilityListWire {
            service: service as u8,
            count: 1,
            ..Default::default()
        };
        list.capabilities[0] = capability as u8;
        args.capabilities[lists] = list;
        lists += 1;
    }
    args.capability_count = lists as u16;
    Ok(args)
}

fn call(args: &RustosServicePolicyArgs) -> Result<(), i32> {
    let status = super::syscall1(
        SYS_RUSTOS_SERVICE_POLICY,
        (args as *const RustosServicePolicyArgs) as u64,
    );
    if status < 0 {
        Err((-status) as i32)
    } else {
        Ok(())
    }
}

/// Installs the policy. Must run after rootd registers its own endpoint (which
/// is what gives it `RootSupervisor`) and before any service is spawned.
pub(super) fn install() {
    let generation = 1;
    let result = build_policy(generation).and_then(|args| call(&args));
    match result {
        Ok(()) => {
            GENERATION.store(generation, Ordering::Release);
            super::debug_line(b"rootd: service policy installed in kernel\n");
        }
        Err(_) => super::fail_closed(b"rootd: fatal service policy install failed\n"),
    }
}

/// Tells the kernel that live process `pid` is `service`. Called where a lease
/// becomes live, before the child is activated.
pub(super) fn bind(pid: u64, service: u64) {
    let generation = GENERATION.load(Ordering::Acquire);
    if generation == 0 || pid == 0 || service == 0 {
        return;
    }
    let args = RustosServicePolicyArgs {
        op: SERVICE_POLICY_OP_BIND_PRINCIPAL,
        generation,
        principal_pid: pid,
        principal_service: service,
        ..RustosServicePolicyArgs::default()
    };
    if call(&args).is_err() {
        // The kernel then falls back to asking rootd for this process, which
        // is counted; a failed bind never widens anything.
        super::debug_line(b"rootd: service principal bind refused\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_policy_is_exactly_the_manifest_routes_and_roles_as_typed_lists() {
        let args = build_policy(1).unwrap();
        for route in &args.routes[..usize::from(args.route_count)] {
            assert!(component_policy::route_allowed(
                u64::from(route.subject),
                u64::from(route.target)
            ));
        }
        let expected = (1..16_u64)
            .flat_map(|s| (1..16_u64).map(move |t| (s, t)))
            .filter(|(s, t)| s != t && component_policy::route_allowed(*s, *t))
            .count();
        assert_eq!(usize::from(args.route_count), expected);
        assert!(expected > 0);
        for entry in &args.capabilities[..usize::from(args.capability_count)] {
            assert_eq!(entry.count, 1, "a role is exactly one capability");
            let typed = rustos_user_abi::syscall::ServiceCapability::from_id(entry.capabilities[0]);
            assert_eq!(
                typed,
                component_policy::service_policy_capability(u64::from(entry.service)),
                "service {}",
                entry.service
            );
        }
        assert!(args.capability_count >= 12, "every role in the manifest is delivered");
    }

    #[test]
    fn the_policy_always_fits_its_wire() {
        assert!(build_policy(7).is_ok());
    }
}
