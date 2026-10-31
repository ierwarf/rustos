use crate::identity::{ObjectIdentity, ObjectKind, ObjectOwner};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HandleOwner {
    /// Generational job tokens have a separate namespace from PS console ids.
    Job,
    Io,
    Compat,
    Ps,
}

/// Provider-facing token retained by a process descriptor entry.
///
/// The pair remains the compatibility identity for older providers. New
/// providers whose slot is proven non-reusable also carry an
/// [`ObjectIdentity`], without changing the process-visible `u64` fd/handle
/// representation or making an unproven token look generational.
#[derive(Clone, Copy, Debug)]
pub struct HandleToken {
    owner: HandleOwner,
    object_id: u64,
    identity: Option<ObjectIdentity>,
}

impl PartialEq for HandleToken {
    fn eq(&self, other: &Self) -> bool {
        self.owner == other.owner && self.object_id == other.object_id
    }
}

impl Eq for HandleToken {}

impl HandleToken {
    pub const fn new(owner: HandleOwner, object_id: u64) -> Self {
        Self {
            owner,
            object_id,
            identity: None,
        }
    }

    /// Adapts a provider token that is allocated once and never reused.
    ///
    /// Such providers use the token as the stable slot and generation `1`.
    /// This is not available for tokens whose allocator/lifetime has not yet
    /// established the non-reuse property.
    pub const fn from_nonreusable_open_description(
        owner: HandleOwner,
        object_id: u64,
    ) -> Option<Self> {
        let object_owner = match owner {
            HandleOwner::Io => ObjectOwner::Io,
            HandleOwner::Compat => ObjectOwner::ServiceProxy,
            HandleOwner::Ps => ObjectOwner::Ps,
            HandleOwner::Job => return None,
        };
        let Some(identity) =
            ObjectIdentity::new(object_owner, ObjectKind::OpenDescription, object_id, 1)
        else {
            return None;
        };
        Some(Self {
            owner,
            object_id,
            identity: Some(identity),
        })
    }

    /// Encodes a complete job identity, including root slot zero. The identity
    /// vocabulary reserves slot zero, so its slot is the job index plus one.
    pub const fn from_job(index: u32, generation: u32) -> Option<Self> {
        let Some(identity) = ObjectIdentity::new(
            ObjectOwner::Ps,
            ObjectKind::Job,
            index as u64 + 1,
            generation as u64,
        ) else {
            return None;
        };
        Some(Self {
            owner: HandleOwner::Job,
            object_id: ((index as u64) << 32) | generation as u64,
            identity: Some(identity),
        })
    }

    pub const fn owner(self) -> HandleOwner {
        self.owner
    }

    pub const fn object_id(self) -> u64 {
        self.object_id
    }

    /// `None` denotes a legacy provider token pending a generation-safe
    /// adapter. Callers must not infer a generation from that absence.
    pub const fn identity(self) -> Option<ObjectIdentity> {
        self.identity
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileHandleRights(u32);

impl FileHandleRights {
    pub const READ: Self = Self(1 << 0);
    pub const WRITE: Self = Self(1 << 1);
    pub const APPEND: Self = Self(1 << 2);
    pub const NONBLOCK: Self = Self(1 << 3);
    pub const TRANSFER: Self = Self(1 << 4);

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn contains(self, right: Self) -> bool {
        self.0 & right.0 == right.0
    }

    pub const fn is_subset_of(self, parent: Self) -> bool {
        self.0 & !parent.0 == 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceHandleRights(u32);

impl DeviceHandleRights {
    pub const READ: Self = Self(1 << 0);
    pub const WRITE: Self = Self(1 << 1);
    pub const IOCTL: Self = Self(1 << 2);
    pub const ADMIN: Self = Self(1 << 3);
    pub const MAP: Self = Self(1 << 4);
    pub const TRANSFER: Self = Self(1 << 5);

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn contains(self, right: Self) -> bool {
        self.0 & right.0 == right.0
    }

    pub const fn is_subset_of(self, parent: Self) -> bool {
        self.0 & !parent.0 == 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SharedRegionRights(u32);

impl SharedRegionRights {
    pub const READ: Self = Self(1 << 0);
    pub const WRITE: Self = Self(1 << 1);
    pub const MAP: Self = Self(1 << 2);
    pub const TRANSFER: Self = Self(1 << 3);

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn contains(self, right: Self) -> bool {
        self.0 & right.0 == right.0
    }

    pub const fn is_subset_of(self, parent: Self) -> bool {
        self.0 & !parent.0 == 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SocketHandleRights(u32);

impl SocketHandleRights {
    pub const SEND: Self = Self(1 << 0);
    pub const RECV: Self = Self(1 << 1);
    pub const PASS_FD: Self = Self(1 << 2);
    pub const TRANSFER: Self = Self(1 << 3);

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn contains(self, right: Self) -> bool {
        self.0 & right.0 == right.0
    }

    pub const fn is_subset_of(self, parent: Self) -> bool {
        self.0 & !parent.0 == 0
    }
}

/// Authority over one job's subtree. Explicit duplication may only attenuate
/// these bits; possession never grants authority over an ancestor or sibling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JobHandleRights(u32);

impl JobHandleRights {
    pub const INSPECT: Self = Self(1 << 0);
    pub const WAIT: Self = Self(1 << 1);
    pub const CREATE_JOB: Self = Self(1 << 2);
    pub const CREATE_PROCESS: Self = Self(1 << 3);
    pub const SET_LIMITS: Self = Self(1 << 4);
    pub const SET_POLICY: Self = Self(1 << 5);
    pub const KILL: Self = Self(1 << 6);
    pub const SET_CRITICAL: Self = Self(1 << 7);
    pub const DUPLICATE: Self = Self(1 << 8);
    pub const TRANSFER: Self = Self(1 << 9);
    /// Rootd-only admission/refund authority; never lent to loaderd.
    pub const ACCOUNT_SESSIONS: Self = Self(1 << 10);
    pub const ALL: Self = Self((1 << 11) - 1);

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn from_bits(bits: u32) -> Option<Self> {
        if bits & !Self::ALL.0 == 0 { Some(Self(bits)) } else { None }
    }

    pub const fn bits(self) -> u32 {
        self.0
    }

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn contains(self, right: Self) -> bool {
        self.0 & right.0 == right.0
    }

    pub const fn is_subset_of(self, parent: Self) -> bool {
        self.0 & !parent.0 == 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HandleRights {
    Console,
    File(FileHandleRights),
    Device(DeviceHandleRights),
    Socket(SocketHandleRights),
    Epoll,
    Memfd(FileHandleRights),
    DisplaySurface(SharedRegionRights),
    Job(JobHandleRights),
}

impl HandleRights {
    /// Returns whether `child` is a typed attenuation of this capability.
    /// Different object kinds are never interchangeable, even when their
    /// private bit layouts happen to overlap.
    pub const fn can_attenuate_to(self, child: Self) -> bool {
        match (self, child) {
            (Self::File(parent), Self::File(child)) | (Self::Memfd(parent), Self::Memfd(child)) => {
                child.is_subset_of(parent)
            }
            (Self::Device(parent), Self::Device(child)) => child.is_subset_of(parent),
            (Self::Socket(parent), Self::Socket(child)) => child.is_subset_of(parent),
            (Self::Job(parent), Self::Job(child)) => child.is_subset_of(parent),
            (Self::DisplaySurface(parent), Self::DisplaySurface(child)) => {
                child.is_subset_of(parent)
            }
            (Self::Console, Self::Console) | (Self::Epoll, Self::Epoll) => true,
            _ => false,
        }
    }

    pub const fn allows_transfer(self) -> bool {
        match self {
            Self::File(rights) | Self::Memfd(rights) => rights.contains(FileHandleRights::TRANSFER),
            Self::Socket(rights) => rights.contains(SocketHandleRights::TRANSFER),
            Self::Device(rights) => rights.contains(DeviceHandleRights::TRANSFER),
            Self::DisplaySurface(rights) => rights.contains(SharedRegionRights::TRANSFER),
            Self::Job(rights) => rights.contains(JobHandleRights::TRANSFER),
            Self::Console | Self::Epoll => false,
        }
    }

    pub const fn allows_shared_map(self) -> bool {
        match self {
            Self::DisplaySurface(rights) => rights.contains(SharedRegionRights::MAP),
            Self::Device(rights) => rights.contains(DeviceHandleRights::MAP),
            _ => false,
        }
    }

    pub const fn allows_read(self) -> bool {
        match self {
            Self::File(rights) | Self::Memfd(rights) => rights.contains(FileHandleRights::READ),
            Self::Device(rights) => rights.contains(DeviceHandleRights::READ),
            Self::DisplaySurface(rights) => rights.contains(SharedRegionRights::READ),
            _ => false,
        }
    }

    pub const fn allows_write(self) -> bool {
        match self {
            Self::File(rights) | Self::Memfd(rights) => rights.contains(FileHandleRights::WRITE),
            Self::Device(rights) => rights.contains(DeviceHandleRights::WRITE),
            Self::DisplaySurface(rights) => rights.contains(SharedRegionRights::WRITE),
            _ => false,
        }
    }

    pub const fn allows_device_ioctl(self) -> bool {
        match self {
            Self::Device(rights) => rights.contains(DeviceHandleRights::IOCTL),
            _ => false,
        }
    }

    pub const fn allows_device_admin(self) -> bool {
        match self {
            Self::Device(rights) => rights.contains(DeviceHandleRights::ADMIN),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DeviceHandleRights, FileHandleRights, HandleOwner, HandleRights, HandleToken, JobHandleRights};
    use crate::identity::{ObjectKind, ObjectOwner};

    #[test]
    fn nonreusable_open_description_adapter_preserves_legacy_token_equality() {
        let token = HandleToken::from_nonreusable_open_description(HandleOwner::Ps, 17)
            .expect("nonzero nonreusable token");
        let identity = token.identity().expect("identity adapter");
        assert_eq!(identity.owner(), ObjectOwner::Ps);
        assert_eq!(identity.kind(), ObjectKind::OpenDescription);
        assert_eq!(identity.slot(), 17);
        assert_eq!(identity.generation(), 1);
        assert_eq!(token, HandleToken::new(HandleOwner::Ps, 17));
        assert!(HandleToken::new(HandleOwner::Ps, 17).identity().is_none());
        assert!(HandleToken::from_nonreusable_open_description(HandleOwner::Ps, 0).is_none());
    }

    #[test]
    fn job_tokens_preserve_generation_and_cannot_alias_console_tokens() {
        let root = HandleToken::from_job(0, 1).unwrap();
        let identity = root.identity().unwrap();
        assert_eq!(identity.kind(), ObjectKind::Job);
        assert_eq!((identity.slot(), identity.generation()), (1, 1));
        assert_eq!(root.owner(), HandleOwner::Job);
        assert_ne!(root, HandleToken::new(HandleOwner::Ps, root.object_id()));
        let recycled = HandleToken::from_job(0, 2).unwrap();
        assert_ne!(root, recycled);
        assert_eq!(recycled.identity().unwrap().generation(), 2);
        assert_eq!(HandleToken::from_job(u32::MAX, u32::MAX).unwrap().object_id(), u64::MAX);
        assert!(HandleToken::from_job(0, 0).is_none());
        assert!(HandleToken::from_nonreusable_open_description(HandleOwner::Job, 1).is_none());
    }

    #[test]
    fn job_rights_only_attenuate_and_transfer_requires_its_own_bit() {
        let rights = JobHandleRights::INSPECT.union(JobHandleRights::DUPLICATE);
        let parent = HandleRights::Job(rights);
        assert!(parent.can_attenuate_to(HandleRights::Job(JobHandleRights::INSPECT)));
        assert!(!parent.can_attenuate_to(HandleRights::Job(rights.union(JobHandleRights::KILL))));
        assert!(!parent.can_attenuate_to(HandleRights::File(FileHandleRights::READ)));
        assert!(!HandleRights::File(FileHandleRights::READ).can_attenuate_to(parent));
        assert!(!parent.allows_transfer());
        assert!(HandleRights::Job(rights.union(JobHandleRights::TRANSFER)).allows_transfer());
        assert_eq!(JobHandleRights::from_bits(rights.bits()), Some(rights));
        assert_eq!(JobHandleRights::from_bits(1 << 10), None);
        assert_eq!(JobHandleRights::from_bits(u32::MAX), None);
        assert_eq!(JobHandleRights::from_bits(0), Some(JobHandleRights::empty()));
        assert!(JobHandleRights::ALL.contains(JobHandleRights::CREATE_PROCESS));
    }

    #[test]
    fn typed_rights_attenuation_rejects_widening_and_kind_substitution() {
        let parent = HandleRights::File(
            FileHandleRights::READ
                .union(FileHandleRights::WRITE)
                .union(FileHandleRights::TRANSFER),
        );
        assert!(parent.can_attenuate_to(HandleRights::File(
            FileHandleRights::READ.union(FileHandleRights::TRANSFER),
        )));
        assert!(
            !parent.can_attenuate_to(HandleRights::File(
                FileHandleRights::READ
                    .union(FileHandleRights::WRITE)
                    .union(FileHandleRights::APPEND)
                    .union(FileHandleRights::TRANSFER),
            ))
        );
        assert!(!parent.can_attenuate_to(HandleRights::Device(DeviceHandleRights::READ)));
    }
}
