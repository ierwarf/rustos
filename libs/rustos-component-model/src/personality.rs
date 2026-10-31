//! Slow-path personality policy. A decision here is never a kernel capability.
//! The broker MUST bind subject PID, Job generation and typed handles first.
//! Never invoke this module from syscall entry, page faults, IPC or scheduling.
use core::cmp::Ordering;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Personality { Native, Linux, Windows }

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SlowRequest {
    ExecutableImage, FileAssociation, MountPolicy, NetworkPolicy,
    CredentialPolicy, NamespacePolicy, CompatibilityProfile,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Principal {
    pub pid: u64,
    pub job_slot: u32,
    pub job_generation: u32,
    pub lease_generation: u64,
}
impl Principal {
    pub fn valid(self) -> bool {
        self.pid != 0 && self.job_generation != 0 && self.lease_generation != 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PolicyKey {
    pub personality: Personality,
    pub operation: SlowRequest,
    pub namespace: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rule {
    pub key: PolicyKey,
    /// An exact owner can grant; wildcard namespace rules can only deny.
    pub owner: Principal,
    pub allow: bool,
    pub revision: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Denial {
    InvalidPrincipal, InvalidRevision, MissingRule, WrongOwner,
    StaleRevision, AmbiguousRule, Capacity, UnsupportedMechanism,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Decision {
    pub revision: u64,
    pub key: PolicyKey,
    pub owner: Principal,
}

/// Configuration classification is deliberately exhaustive. Kernel mechanisms
/// may not be reclassified by adding a slow policy rule.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mechanism {
    IpcTransfer, IpcReply, Scheduler, Futex, Clock, ProcessIdentity,
    SyscallEntry, UserCopy, PageFault, PageTableCommit, TlbShootdown,
    JobAccounting, CapabilityTransfer,
}
impl Mechanism {
    pub fn ring0(self) -> bool { true }
}

/// Fixed-size, fail-closed policy snapshot. The supervisor publishes a new
/// instance only after all records pass validation; no partial replacement.
pub struct PolicySnapshot<const N: usize> {
    rules: [Option<Rule>; N],
    revision: u64,
}
impl<const N: usize> PolicySnapshot<N> {
    pub const fn empty() -> Self {
        Self { rules: [None; N], revision: 0 }
    }
    pub fn revision(&self) -> u64 { self.revision }
    pub fn replace(&mut self, next: &[Rule], revision: u64) -> Result<(), Denial> {
        if revision == 0 || revision <= self.revision { return Err(Denial::StaleRevision); }
        if next.len() > N { return Err(Denial::Capacity); }
        for (index, rule) in next.iter().enumerate() {
            if !rule.owner.valid() || rule.revision != revision {
                return Err(Denial::InvalidRevision);
            }
            if rule.key.namespace == 0 && rule.allow {
                return Err(Denial::AmbiguousRule);
            }
            for other in &next[..index] {
                if rule.key == other.key { return Err(Denial::AmbiguousRule); }
            }
        }
        let mut staged = [None; N];
        for (i, rule) in next.iter().enumerate() { staged[i] = Some(*rule); }
        self.rules = staged;
        self.revision = revision;
        Ok(())
    }
    pub fn authorize(
        &self, key: PolicyKey, subject: Principal, minimum_revision: u64,
    ) -> Result<Decision, Denial> {
        if !subject.valid() { return Err(Denial::InvalidPrincipal); }
        if minimum_revision == 0 || self.revision < minimum_revision {
            return Err(Denial::StaleRevision);
        }
        let mut found = None;
        for item in self.rules.iter().flatten() {
            if item.key == key {
                found = Some(*item);
                break;
            }
        }
        let rule = found.ok_or(Denial::MissingRule)?;
        if !rule.allow { return Err(Denial::MissingRule); }
        if rule.owner != subject { return Err(Denial::WrongOwner); }
        Ok(Decision { revision: self.revision, key, owner: subject })
    }
    /// No wildcard permits an action: a wildcard deny only narrows access.
    pub fn authorize_with_namespace_deny(
        &self, key: PolicyKey, subject: Principal, minimum_revision: u64,
    ) -> Result<Decision, Denial> {
        let exact = self.authorize(key, subject, minimum_revision)?;
        if key.namespace != 0 {
            for rule in self.rules.iter().flatten() {
                if rule.key.personality == key.personality
                    && rule.key.operation == key.operation
                    && rule.key.namespace == 0 && !rule.allow {
                    return Err(Denial::MissingRule);
                }
            }
        }
        Ok(exact)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProfileVersion {
    pub major: u16, pub minor: u16, pub patch: u16,
}
impl ProfileVersion {
    pub fn compare(self, other: Self) -> Ordering {
        (self.major, self.minor, self.patch).cmp(&(other.major, other.minor, other.patch))
    }
}

/// Version negotiation is a slow control operation. Never pretend a binary
/// of unknown major ABI is compatible by falling back to a default profile.
pub fn negotiate(
    requested: ProfileVersion, supported: &[ProfileVersion],
) -> Option<ProfileVersion> {
    supported.iter().copied()
        .filter(|v| v.major == requested.major && v.compare(requested) != Ordering::Greater)
        .max_by(|a, b| a.compare(*b))
}

#[cfg(test)]
mod tests {
    use super::*;
    const OWNER: Principal = Principal {
        pid: 14, job_slot: 2, job_generation: 7, lease_generation: 9,
    };
    const KEY: PolicyKey = PolicyKey {
        personality: Personality::Linux,
        operation: SlowRequest::MountPolicy, namespace: 42,
    };
    #[test]
    fn replacement_is_atomic_and_generation_scoped() {
        let mut p = PolicySnapshot::<2>::empty();
        let one = Rule { key: KEY, owner: OWNER, allow: true, revision: 1 };
        p.replace(&[one], 1).unwrap();
        assert_eq!(p.replace(&[one, one], 2), Err(Denial::InvalidRevision));
        assert_eq!(p.revision(), 1);
        assert!(p.authorize(KEY, OWNER, 1).is_ok());
        let stale = Principal { lease_generation: 8, ..OWNER };
        assert_eq!(p.authorize(KEY, stale, 1), Err(Denial::WrongOwner));
    }
    #[test]
    fn no_unknown_abi_major_fallback() {
        assert_eq!(negotiate(ProfileVersion {major: 3, minor: 0, patch: 0},
          &[ProfileVersion { major: 2, minor: 9, patch: 0 }]), None);
    }
}
