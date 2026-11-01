//! Rootd's per-personality slow-request policy (canonical IDL 523).
//!
//! `rustos_component_model::personality::PolicySnapshot` is the model; this is
//! its production owner. Rules are exact-incarnation: a rule is owned by one
//! live `Principal` (pid, Job slot and generation, lease generation), so the
//! restart of a service silently revokes everything its predecessor held. The
//! manifest below says which service may act for which personality; the
//! snapshot is rebuilt from it for the live principals and republished whole
//! (new revision, all-or-nothing) whenever that set changes. This is never
//! consulted from syscall entry, IPC, scheduling or fault paths.
use alloc::vec::Vec;
use rustos_component_model::personality::{
    Denial, PolicyKey, PolicySnapshot, Personality, Principal, Rule, SlowRequest,
};
use rustos_user_abi::syscall::IPC_SERVICE_LOADERD;

/// The one default namespace every personality starts with.
pub(super) const DEFAULT_NAMESPACE: u64 = 1;
const MAX_RULES: usize = 32;
const EACCES: i32 = 13;
const EINVAL: i32 = 22;
const ESTALE: i32 = 116;

#[derive(Clone, Copy)]
pub(super) struct Grant {
    pub service: u64,
    pub personality: Personality,
    pub request: SlowRequest,
    pub namespace: u64,
}

/// loaderd admits and launches both executable formats; nothing else here
/// creates processes from foreign images, so nothing else is granted.
pub(super) const GRANTS: [Grant; 2] = [
    Grant {
        service: IPC_SERVICE_LOADERD,
        personality: Personality::Linux,
        request: SlowRequest::ExecutableImage,
        namespace: DEFAULT_NAMESPACE,
    },
    Grant {
        service: IPC_SERVICE_LOADERD,
        personality: Personality::Windows,
        request: SlowRequest::ExecutableImage,
        namespace: DEFAULT_NAMESPACE,
    },
];

pub(super) struct PersonalityPolicy {
    snapshot: PolicySnapshot<MAX_RULES>,
    published: Vec<Rule>,
}

impl PersonalityPolicy {
    pub(super) const fn new() -> Self {
        Self { snapshot: PolicySnapshot::empty(), published: Vec::new() }
    }

    /// Rebuild from the manifest for the live `principals` (service, owner).
    /// Republishes only when the rule set actually changed.
    pub(super) fn refresh(&mut self, principals: &[(u64, Principal)]) -> Result<(), i32> {
        let revision = self.snapshot.revision().saturating_add(1);
        let mut wanted = Vec::new();
        for grant in GRANTS {
            for (service, owner) in principals {
                if *service == grant.service && owner.valid() {
                    wanted.try_reserve(1).map_err(|_| 12)?;
                    wanted.push(Rule {
                        key: PolicyKey {
                            personality: grant.personality,
                            operation: grant.request,
                            namespace: grant.namespace,
                        },
                        owner: *owner,
                        allow: true,
                        revision,
                    });
                }
            }
        }
        let unchanged = wanted.len() == self.published.len()
            && wanted.iter().zip(&self.published).all(|(a, b)| {
                Rule { revision: 0, ..*a } == Rule { revision: 0, ..*b }
            });
        if unchanged {
            return Ok(());
        }
        self.snapshot.replace(&wanted, revision).map_err(errno)?;
        self.published = wanted;
        Ok(())
    }

    pub(super) fn authorize(&self, key: PolicyKey, subject: Principal) -> Result<u64, i32> {
        self.snapshot
            .authorize_with_namespace_deny(key, subject, 1)
            .map(|decision| decision.revision)
            .map_err(errno)
    }
}

fn errno(denial: Denial) -> i32 {
    match denial {
        Denial::InvalidPrincipal | Denial::InvalidRevision | Denial::AmbiguousRule => EINVAL,
        Denial::StaleRevision => ESTALE,
        Denial::Capacity => 28,
        Denial::MissingRule | Denial::WrongOwner | Denial::UnsupportedMechanism => EACCES,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loaderd(pid: u64) -> (u64, Principal) {
        (IPC_SERVICE_LOADERD, Principal { pid, job_slot: 1, job_generation: 1, lease_generation: pid })
    }
    fn key(personality: Personality) -> PolicyKey {
        PolicyKey { personality, operation: SlowRequest::ExecutableImage, namespace: DEFAULT_NAMESPACE }
    }

    #[test]
    fn the_live_loader_may_launch_both_formats_and_nothing_else() {
        let mut policy = PersonalityPolicy::new();
        let (service, owner) = loaderd(7);
        policy.refresh(&[(service, owner)]).unwrap();
        assert!(policy.authorize(key(Personality::Linux), owner).is_ok());
        assert!(policy.authorize(key(Personality::Windows), owner).is_ok());
        assert_eq!(policy.authorize(key(Personality::Native), owner), Err(EACCES));
        let mount = PolicyKey { operation: SlowRequest::MountPolicy, ..key(Personality::Linux) };
        assert_eq!(policy.authorize(mount, owner), Err(EACCES));
        let other_ns = PolicyKey { namespace: 2, ..key(Personality::Linux) };
        assert_eq!(policy.authorize(other_ns, owner), Err(EACCES));
    }

    #[test]
    fn a_restarted_service_does_not_inherit_its_predecessors_rules() {
        let mut policy = PersonalityPolicy::new();
        let (service, old) = loaderd(7);
        policy.refresh(&[(service, old)]).unwrap();
        let (_, new) = loaderd(9);
        assert_eq!(policy.authorize(key(Personality::Linux), new), Err(EACCES));
        policy.refresh(&[(service, new)]).unwrap();
        assert!(policy.authorize(key(Personality::Linux), new).is_ok());
        assert_eq!(
            policy.authorize(key(Personality::Linux), old),
            Err(EACCES),
            "the dead incarnation's rules are gone with the republish"
        );
    }

    #[test]
    fn an_unchanged_live_set_does_not_advance_the_revision() {
        let mut policy = PersonalityPolicy::new();
        let (service, owner) = loaderd(7);
        policy.refresh(&[(service, owner)]).unwrap();
        let first = policy.authorize(key(Personality::Linux), owner).unwrap();
        policy.refresh(&[(service, owner)]).unwrap();
        assert_eq!(policy.authorize(key(Personality::Linux), owner).unwrap(), first);
    }

    #[test]
    fn a_service_without_a_grant_gets_no_rule_and_an_invalid_principal_is_refused() {
        let mut policy = PersonalityPolicy::new();
        let stranger = Principal { pid: 5, job_slot: 1, job_generation: 1, lease_generation: 5 };
        policy.refresh(&[(IPC_SERVICE_LOADERD + 1, stranger)]).unwrap();
        assert_eq!(policy.authorize(key(Personality::Linux), stranger), Err(ESTALE));
        let invalid = Principal { pid: 0, ..stranger };
        assert_eq!(policy.authorize(key(Personality::Linux), invalid), Err(EINVAL));
    }
}
