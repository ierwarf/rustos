//! Canonical kernel object and capability identity vocabulary.
//!
//! Kernel object registries may use different storage layouts, but an
//! authority that crosses a subsystem boundary must always name its owner,
//! kind, slot, and non-reusable generation. Provider lease and derivation
//! revoke epochs deliberately remain separate from that storage generation.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObjectOwner {
    Ps,
    Ipc,
    Mm,
    Io,
    ServiceProxy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObjectKind {
    Process,
    Task,
    OpenDescription,
    Endpoint,
    Reply,
    Transfer,
    VmObject,
    Frame,
    SchedulingContext,
    DvmTransport,
    FaultToken,
    LifecycleToken,
}

/// Internal identity for one live kernel or service-proxy object.
///
/// `slot` and `generation` are intentionally distinct. A registry may reuse
/// a slot only after advancing its generation; a caller must never turn a
/// slot-only value back into live authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ObjectIdentity {
    owner: ObjectOwner,
    kind: ObjectKind,
    slot: u64,
    generation: u64,
}

impl ObjectIdentity {
    pub const fn new(
        owner: ObjectOwner,
        kind: ObjectKind,
        slot: u64,
        generation: u64,
    ) -> Option<Self> {
        if slot == 0 || generation == 0 {
            return None;
        }
        Some(Self {
            owner,
            kind,
            slot,
            generation,
        })
    }

    pub const fn owner(self) -> ObjectOwner {
        self.owner
    }

    pub const fn kind(self) -> ObjectKind {
        self.kind
    }

    pub const fn slot(self) -> u64 {
        self.slot
    }

    pub const fn generation(self) -> u64 {
        self.generation
    }
}

/// Service/provider lease and derivation-tree revoke state for a capability.
/// Neither value is an object-slot generation: a provider restart advances a
/// lease even when the backing slot survives, while a revoke invalidates a
/// derivation tree without granting a new provider lease.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapabilityEpochs {
    lease_epoch: u64,
    revoke_epoch: u64,
}

impl CapabilityEpochs {
    pub const fn new(lease_epoch: u64, revoke_epoch: u64) -> Option<Self> {
        if lease_epoch == 0 || revoke_epoch == 0 {
            return None;
        }
        Some(Self {
            lease_epoch,
            revoke_epoch,
        })
    }

    pub const fn lease_epoch(self) -> u64 {
        self.lease_epoch
    }

    pub const fn revoke_epoch(self) -> u64 {
        self.revoke_epoch
    }

    /// A derived capability must remain inside the exact provider lease and
    /// revoke tree that authorized its parent. Rights attenuation is checked
    /// by the typed rights owner alongside this generic identity check.
    pub const fn permits_derivation_to(self, child: Self) -> bool {
        self.lease_epoch == child.lease_epoch && self.revoke_epoch == child.revoke_epoch
    }
}

#[cfg(test)]
mod tests {
    use super::{CapabilityEpochs, ObjectIdentity, ObjectKind, ObjectOwner};

    #[test]
    fn identity_rejects_zero_slot_or_generation() {
        assert!(ObjectIdentity::new(ObjectOwner::Ipc, ObjectKind::Transfer, 0, 1).is_none());
        assert!(ObjectIdentity::new(ObjectOwner::Ipc, ObjectKind::Transfer, 1, 0).is_none());
        assert_eq!(
            ObjectIdentity::new(ObjectOwner::Ipc, ObjectKind::Transfer, 7, 9)
                .expect("valid object identity")
                .generation(),
            9
        );
    }

    #[test]
    fn capability_epochs_keep_lease_and_revoke_distinct() {
        let parent = CapabilityEpochs::new(3, 11).expect("valid parent epochs");
        assert!(
            parent.permits_derivation_to(
                CapabilityEpochs::new(3, 11).expect("same authority epochs")
            )
        );
        assert!(!parent.permits_derivation_to(
            CapabilityEpochs::new(4, 11).expect("different provider lease")
        ));
        assert!(
            !parent.permits_derivation_to(
                CapabilityEpochs::new(3, 12).expect("different revoke tree")
            )
        );
    }
}
